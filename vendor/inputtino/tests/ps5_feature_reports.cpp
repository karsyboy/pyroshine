// Hardware-independent checks of the DualSense UHID event handler.
// `on_uhid_event` is file-static, so this test compiles its translation unit
// and reads the replies it writes to a pipe instead of /dev/uhid.
//
//   ps5-feature-report-test supported    byte-exact supported replies (USB and
//                                        Bluetooth, including the CRC trailer)
//   ps5-feature-report-test unsupported  every other report number and type
//                                        gets an error reply without a payload
//   ps5-feature-report-test output       output reports are parsed only when
//                                        they contain the whole common block
//   ps5-feature-report-test fuzz         seeded random requests and output
//                                        reports (meaningful under ASan/UBSan)
//
// Review 2026-10-05 BUG-001: the Bluetooth error path used to compute a CRC
// over `size - 4` with size 0 and overread the stack.
#ifdef NDEBUG
#undef NDEBUG
#endif
#include "../src/uhid/joypad_ps5.cpp"

#include <cassert>
#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <vector>

namespace {

uhid_event query(bool bluetooth, uint8_t rnum, uint8_t rtype, uint32_t id) {
  int fds[2];
  assert(pipe(fds) == 0);
  auto state = std::make_shared<inputtino::PS5JoypadState>();
  state->is_bluetooth = bluetooth;
  uhid_event request{};
  request.type = UHID_GET_REPORT;
  request.u.get_report.id = id;
  request.u.get_report.rnum = rnum;
  request.u.get_report.rtype = rtype;
  inputtino::on_uhid_event(state, request, fds[1]);
  uhid_event reply{};
  // One uhid_event is far below the pipe capacity, so the write never blocked.
  auto received = read(fds[0], &reply, sizeof(reply));
  close(fds[0]);
  close(fds[1]);
  assert(received == static_cast<ssize_t>(sizeof(reply)));
  assert(reply.type == UHID_GET_REPORT_REPLY);
  assert(reply.u.get_report_reply.id == id);
  return reply;
}

struct Supported {
  uint8_t rnum;
  const unsigned char *table;
  size_t size;
  // Golden little-endian Bluetooth CRC trailer of the default-MAC reply.
  uint32_t bluetooth_crc;
};

const Supported SUPPORTED[] = {
    {uhid::PS5_REPORT_TYPES::CALIBRATION, uhid::ps5_calibration_info, sizeof(uhid::ps5_calibration_info), 0x2bca938d},
    {uhid::PS5_REPORT_TYPES::PAIRING_INFO, uhid::ps5_pairing_info, sizeof(uhid::ps5_pairing_info), 0x67d0649c},
    {uhid::PS5_REPORT_TYPES::FIRMWARE_INFO, uhid::ps5_firmware_info, sizeof(uhid::ps5_firmware_info), 0x57d97982},
};

bool is_supported(unsigned rnum) {
  for (const auto &report : SUPPORTED)
    if (report.rnum == rnum)
      return true;
  return false;
}

void check_supported() {
  const inputtino::PS5JoypadState defaults{};
  for (bool bluetooth : {false, true}) {
    for (const auto &report : SUPPORTED) {
      auto reply = query(bluetooth, report.rnum, UHID_FEATURE_REPORT, 0x1200 + report.rnum);
      assert(reply.u.get_report_reply.err == 0);
      assert(reply.u.get_report_reply.size == report.size);
      std::vector<unsigned char> expected(report.table, report.table + report.size);
      if (report.rnum == uhid::PS5_REPORT_TYPES::PAIRING_INFO) {
        std::reverse_copy(std::begin(defaults.mac_address), std::end(defaults.mac_address), expected.begin() + 1);
      }
      if (bluetooth) {
        uint32_t crc = htole32(CRC32(expected.data(), report.size - 4, uhid::PS_FEATURE_CRC32));
        assert(crc == htole32(report.bluetooth_crc));
        std::memcpy(expected.data() + report.size - 4, &crc, 4);
      }
      assert(std::memcmp(reply.u.get_report_reply.data, expected.data(), report.size) == 0);
    }
  }
}

void check_unsupported() {
  // USB first: a Bluetooth failure must not hide the USB result.
  for (bool bluetooth : {false, true}) {
    for (uint8_t rtype : {UHID_FEATURE_REPORT, UHID_OUTPUT_REPORT, UHID_INPUT_REPORT}) {
      for (unsigned rnum = 0; rnum < 256; ++rnum) {
        if (rtype == UHID_FEATURE_REPORT && is_supported(rnum))
          continue;
        auto reply = query(bluetooth, static_cast<uint8_t>(rnum), rtype, 0x3400 + rnum);
        if (reply.u.get_report_reply.err == 0 || reply.u.get_report_reply.size != 0) {
          std::fprintf(stderr,
                       "review 2026-10-05 BUG-001: unsupported %s report 0x%02x (type %u) replied err=%u size=%u\n",
                       bluetooth ? "Bluetooth" : "USB", rnum, rtype, reply.u.get_report_reply.err,
                       reply.u.get_report_reply.size);
          std::abort();
        }
        // The error reply carries no payload bytes either.
        for (size_t b = 0; b < UHID_DATA_MAX; ++b)
          assert(reply.u.get_report_reply.data[b] == 0);
      }
    }
  }
}

struct Feedback {
  std::vector<std::pair<int, int>> rumble;
  std::vector<std::array<int, 3>> led;
};

std::shared_ptr<inputtino::PS5JoypadState> recording_state(Feedback &feedback) {
  auto state = std::make_shared<inputtino::PS5JoypadState>();
  state->on_rumble = [&feedback](int left, int right) { feedback.rumble.emplace_back(left, right); };
  state->on_led = [&feedback](int r, int g, int b) { feedback.led.push_back({r, g, b}); };
  return state;
}

/// An output report whose common block (at `offset`) asks for rumble 255/128
/// and lightbar 1/2/3, truncated to `size` bytes.
uhid_event output_event(uint8_t report_id, uint8_t bt_flags, size_t offset, size_t size) {
  uhid_event ev{};
  ev.type = UHID_OUTPUT;
  ev.u.output.size = static_cast<__u16>(size);
  ev.u.output.data[0] = report_id;
  if (report_id == uhid::DS_OUTPUT_REPORT_BT)
    ev.u.output.data[1] = bt_flags;
  uhid::dualsense_output_report_common common{};
  common.valid_flag0 = uhid::MOTOR_OR_COMPATIBLE_VIBRATION;
  common.valid_flag1 = uhid::LIGHTBAR_ENABLE;
  common.motor_left = 255;
  common.motor_right = 128;
  common.lightbar_red = 1;
  common.lightbar_green = 2;
  common.lightbar_blue = 3;
  std::memcpy(&ev.u.output.data[offset], &common, sizeof(common));
  return ev;
}

void check_output() {
  constexpr size_t COMMON = sizeof(uhid::dualsense_output_report_common);
  static_assert(COMMON == 47);
  struct Case {
    const char *name;
    uint8_t report_id;
    uint8_t bt_flags;
    size_t offset;
  };
  // USB (hid-playstation and SDL), SDL Bluetooth (EnableHID set) and kernel
  // Bluetooth (EnableHID clear, followed by a tag byte).
  const Case cases[] = {
      {"usb", uhid::DS_OUTPUT_REPORT_USB, 0, 1},
      {"bt-sdl", uhid::DS_OUTPUT_REPORT_BT, 0x02, 2},
      {"bt-kernel", uhid::DS_OUTPUT_REPORT_BT, 0x00, 3},
  };
  for (const auto &c : cases) {
    for (size_t size : {c.offset + COMMON, size_t{78}, c.offset + COMMON - 1, size_t{1}, size_t{0}}) {
      Feedback feedback;
      inputtino::on_uhid_event(recording_state(feedback), output_event(c.report_id, c.bt_flags, c.offset, size), -1);
      const bool complete = size >= c.offset + COMMON;
      if (complete) {
        assert(feedback.rumble.size() == 1);
        assert(feedback.rumble[0].first == 0xFFFF);
        assert(feedback.rumble[0].second == static_cast<int>((128 / 255.0f) * 0xFFFF));
        assert(feedback.led.size() == 1 && feedback.led[0] == (std::array<int, 3>{1, 2, 3}));
      } else if (!feedback.rumble.empty() || !feedback.led.empty()) {
        std::fprintf(stderr, "%s: truncated %zu-byte output report was parsed\n", c.name, size);
        std::abort();
      }
    }
  }
  // Unknown output report ids are ignored.
  Feedback feedback;
  inputtino::on_uhid_event(recording_state(feedback), output_event(0x7f, 0, 1, 78), -1);
  assert(feedback.rumble.empty() && feedback.led.empty());
}

struct Rng {
  uint64_t state;
  uint32_t next() {
    state = state * 6364136223846793005ull + 1442695040888963407ull;
    return static_cast<uint32_t>(state >> 33);
  }
};

void fuzz() {
  Rng rng{0x5eed};
  for (int i = 0; i < 20000; ++i) {
    const bool bluetooth = rng.next() & 1;
    const uint8_t rnum = rng.next();
    const uint8_t rtype = rng.next() % 4;
    auto reply = query(bluetooth, rnum, rtype, i);
    const bool supported = rtype == UHID_FEATURE_REPORT && is_supported(rnum);
    assert(supported == (reply.u.get_report_reply.err == 0));
    assert(reply.u.get_report_reply.size <= UHID_DATA_MAX);

    uhid_event ev{};
    ev.type = UHID_OUTPUT;
    ev.u.output.size = static_cast<__u16>(rng.next() % (UHID_DATA_MAX + 64)); // including oversized claims
    ev.u.output.data[0] = (rng.next() & 1) ? uhid::DS_OUTPUT_REPORT_BT : uhid::DS_OUTPUT_REPORT_USB;
    for (size_t b = 1; b < 128; ++b)
      ev.u.output.data[b] = rng.next();
    Feedback feedback;
    inputtino::on_uhid_event(recording_state(feedback), ev, -1);
  }
}

} // namespace

int main(int argc, char **argv) {
  const std::string mode = argc > 1 ? argv[1] : "";
  if (mode == "supported") {
    check_supported();
  } else if (mode == "unsupported") {
    check_unsupported();
  } else if (mode == "output") {
    check_output();
  } else if (mode == "fuzz") {
    fuzz();
  } else {
    std::fprintf(stderr, "usage: %s supported|unsupported|output|fuzz\n", argv[0]);
    return 2;
  }
  std::printf("ps5 uhid events (%s): ok\n", mode.c_str());
  return 0;
}
