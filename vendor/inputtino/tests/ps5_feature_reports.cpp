// Hardware-independent checks of the DualSense UHID feature-report handler.
// `on_uhid_event` is file-static, so this test compiles its translation unit
// and reads the reply it writes to a pipe instead of /dev/uhid.
//
//   ps5-feature-report-test supported    byte-exact supported replies (USB and
//                                        Bluetooth, including the CRC trailer)
//   ps5-feature-report-test unsupported  every other report number yields an
//                                        error reply without a payload
//
// The unsupported mode is a known defect (review 2026-10-05 BUG-001): the
// Bluetooth error path still computes a CRC over `size - 4` with size 0 and
// overreads the stack. Build with -fsanitize=address,undefined to see it.
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

uhid_event query(bool bluetooth, uint8_t rnum, uint32_t id) {
  int fds[2];
  assert(pipe(fds) == 0);
  auto state = std::make_shared<inputtino::PS5JoypadState>();
  state->is_bluetooth = bluetooth;
  uhid_event request{};
  request.type = UHID_GET_REPORT;
  request.u.get_report.id = id;
  request.u.get_report.rnum = rnum;
  request.u.get_report.rtype = UHID_FEATURE_REPORT;
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
      auto reply = query(bluetooth, report.rnum, 0x1200 + report.rnum);
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
    for (unsigned rnum = 0; rnum < 256; ++rnum) {
      if (is_supported(rnum))
        continue;
      auto reply = query(bluetooth, static_cast<uint8_t>(rnum), 0x3400 + rnum);
      if (reply.u.get_report_reply.err == 0 || reply.u.get_report_reply.size != 0) {
        std::fprintf(stderr,
                     "review 2026-10-05 BUG-001: unsupported %s report 0x%02x replied err=%u size=%u\n",
                     bluetooth ? "Bluetooth" : "USB", rnum, reply.u.get_report_reply.err,
                     reply.u.get_report_reply.size);
        std::abort();
      }
    }
  }
}

} // namespace

int main(int argc, char **argv) {
  const std::string mode = argc > 1 ? argv[1] : "";
  if (mode == "supported") {
    check_supported();
  } else if (mode == "unsupported") {
    check_unsupported();
  } else {
    std::fprintf(stderr, "usage: %s supported|unsupported\n", argv[0]);
    return 2;
  }
  std::printf("ps5 feature reports (%s): ok\n", mode.c_str());
  return 0;
}
