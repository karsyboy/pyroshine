// Native controller thread and lifetime checks on real /dev/uhid and
// /dev/uinput devices (review 2026-10-05 STAB-005). Run under TSan and ASan:
//
//   cmake -DINPUTTINO_DEVICE_TESTS=ON -DCMAKE_CXX_FLAGS=-fsanitize=thread ...
//
// For each joypad type, setters run on two threads while the device's own
// report/listener threads run, callbacks are replaced concurrently and
// feedback is injected through the kernel. After the destructor returns, the
// device's threads must be gone (thread and fd counts back to baseline), its
// kernel node removed, and no callback may run afterwards.
#ifdef NDEBUG
#undef NDEBUG
#endif
#include <inputtino/input.hpp>

#include <atomic>
#include <cassert>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <linux/input.h>
#include <string>
#include <thread>
#include <unistd.h>
#include <vector>

using namespace std::chrono_literals;

namespace {

size_t count_entries(const char *dir) {
  size_t n = 0;
  for ([[maybe_unused]] auto &entry : std::filesystem::directory_iterator(dir))
    ++n;
  return n;
}

struct Baseline {
  size_t threads = count_entries("/proc/self/task");
  size_t fds = count_entries("/proc/self/fd");

  void check(const char *what) const {
    // Thread exit is observed through /proc shortly after join returns.
    for (int i = 0; i < 100 && count_entries("/proc/self/task") != threads; ++i)
      std::this_thread::sleep_for(1ms);
    auto now_threads = count_entries("/proc/self/task");
    auto now_fds = count_entries("/proc/self/fd");
    if (now_threads != threads || now_fds != fds) {
      std::fprintf(stderr, "%s: threads %zu -> %zu, fds %zu -> %zu after destruction\n", what, threads, now_threads,
                   fds, now_fds);
      std::abort();
    }
  }
};

struct Callbacks {
  std::atomic<bool> destroyed = false;
  std::atomic<int> after_destroy = 0;
  std::atomic<int> calls = 0;
  void hit() {
    calls++;
    if (destroyed)
      after_destroy++;
  }
};

std::string hidraw_for(const std::string &mac) {
  for (auto &entry : std::filesystem::directory_iterator("/sys/class/hidraw")) {
    FILE *f = std::fopen((entry.path() / "device" / "uevent").c_str(), "r");
    if (!f)
      continue;
    char line[256];
    bool match = false;
    while (std::fgets(line, sizeof(line), f))
      match |= std::string(line).find("HID_UNIQ=" + mac) == 0;
    std::fclose(f);
    if (match)
      return "/dev/" + entry.path().filename().string();
  }
  return {};
}

template <typename Pad> void hammer(Pad &pad, std::atomic<bool> &stop) {
  unsigned buttons = 0;
  while (!stop) {
    pad.set_pressed_buttons(buttons++ & 0xffff);
    pad.set_stick(inputtino::Joypad::LS, static_cast<short>(buttons), static_cast<short>(-buttons));
    pad.set_triggers(static_cast<int16_t>(buttons & 0xff), static_cast<int16_t>((buttons >> 1) & 0xff));
  }
}

void dualsense(int rounds) {
  for (int round = 0; round < rounds; ++round) {
    Baseline baseline;
    Callbacks callbacks;
    std::string node;
    std::string mac;
    std::chrono::steady_clock::duration destroy_time;
    {
      auto created = inputtino::PS5Joypad::create(
          {.name = "Lifetime test pad", .vendor_id = 0x054c, .product_id = 0x0ce6, .version = 0x8111});
      if (!created) {
        std::fprintf(stderr, "cannot create a UHID DualSense: %s\n", created.getErrorMessage().c_str());
        std::exit(77);
      }
      auto pad = std::move(*created);
      for (int i = 0; i < 200 && node.empty(); ++i) {
        node = hidraw_for(pad.get_mac_address());
        std::this_thread::sleep_for(5ms);
      }
      assert(!node.empty());
      std::atomic<bool> stop = false;
      std::thread a([&] { hammer(pad, stop); });
      std::thread b([&] {
        while (!stop) {
          pad.set_on_rumble([&](int, int) { callbacks.hit(); });
          pad.set_on_led([&](int, int, int) { callbacks.hit(); });
          pad.set_on_trigger_effect([&](const auto &) { callbacks.hit(); });
          pad.place_finger(0, 100, 200);
          pad.release_finger(0);
          pad.set_motion(inputtino::PS5Joypad::GYROSCOPE, 1, 2, 3);
        }
      });
      // Kernel-originated feedback while everything runs.
      int fd = -1;
      for (int i = 0; i < 100 && fd < 0; ++i) {
        fd = open(node.c_str(), O_RDWR | O_NONBLOCK);
        if (fd < 0)
          std::this_thread::sleep_for(10ms);
      }
      assert(fd >= 0);
      unsigned char report[48] = {0x02, 0x01 | 0x04 | 0x08, 0x04, 0x40, 0x80};
      for (int i = 0; i < 50; ++i) {
        report[3] = static_cast<unsigned char>(i);
        (void)!write(fd, report, sizeof(report));
        std::this_thread::sleep_for(1ms);
      }
      close(fd);
      stop = true;
      a.join();
      b.join();
      mac = pad.get_mac_address();
      auto started = std::chrono::steady_clock::now();
      {
        auto doomed = std::move(pad);
      }
      destroy_time = std::chrono::steady_clock::now() - started;
      callbacks.destroyed = true;
    }
    std::this_thread::sleep_for(20ms);
    assert(callbacks.after_destroy == 0);
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(destroy_time).count();
    if (ms >= 250) {
      std::fprintf(stderr, "DualSense: destruction took %lld ms\n", static_cast<long long>(ms));
      std::abort();
    }
    if (!hidraw_for(mac).empty()) {
      std::fprintf(stderr, "DualSense: the HID device survived its destructor\n");
      std::abort();
    }
    baseline.check("DualSense");
  }
}

template <typename Pad> void uinput_pad(const char *what, uint16_t vendor, uint16_t product, int rounds) {
  for (int round = 0; round < rounds; ++round) {
    Baseline baseline;
    Callbacks callbacks;
    std::vector<std::string> nodes;
    std::chrono::steady_clock::duration destroy_time;
    {
      auto created = Pad::create({.name = what, .vendor_id = vendor, .product_id = product, .version = 0x114});
      if (!created) {
        std::fprintf(stderr, "cannot create a uinput %s: %s\n", what, created.getErrorMessage().c_str());
        std::exit(77);
      }
      auto pad = std::move(*created);
      nodes = pad.get_nodes();
      assert(!nodes.empty());
      std::atomic<bool> stop = false;
      std::thread a([&] { hammer(pad, stop); });
      std::thread b([&] {
        while (!stop)
          pad.set_on_rumble([&](int, int) { callbacks.hit(); });
      });
      std::this_thread::sleep_for(50ms);
      stop = true;
      a.join();
      b.join();
      // Destroy immediately after creation-time sleep and during polling.
      auto started = std::chrono::steady_clock::now();
      {
        auto doomed = std::move(pad);
      }
      destroy_time = std::chrono::steady_clock::now() - started;
      callbacks.destroyed = true;
    }
    assert(callbacks.after_destroy == 0);
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(destroy_time).count();
    if (ms >= 250) {
      std::fprintf(stderr, "%s: destruction took %lld ms\n", what, static_cast<long long>(ms));
      std::abort();
    }
    for (auto &node : nodes)
      assert(!std::filesystem::exists(node));
    baseline.check(what);
  }
}

} // namespace

int main(int argc, char **argv) {
  int rounds = argc > 1 ? std::atoi(argv[1]) : 5;
  // Sanitizer runtimes start their own helper thread with the first thread;
  // let that happen before any baseline is taken.
  std::thread([] {}).join();
  dualsense(rounds);
  uinput_pad<inputtino::XboxOneJoypad>("Xbox", 0x045e, 0x02ea, rounds);
  uinput_pad<inputtino::XboxOneJoypad>("Xbox Elite", 0x045e, 0x0b00, rounds);
  uinput_pad<inputtino::SwitchJoypad>("Switch", 0x057e, 0x2009, rounds);
  std::printf("joypad lifetimes: ok (%d rounds)\n", rounds);
  return 0;
}
