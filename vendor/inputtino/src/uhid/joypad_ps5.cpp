#include <algorithm>
#include <climits>
#include <cmath>
#include <cstddef>
#include <cstring>
#include <crc32.hpp>
#include <endian.h>
#include <filesystem>
#include <fstream>
#include <inputtino/input.hpp>
#include <iomanip>
#include <optional>
#include <random>
#include <uhid/protected_types.hpp>
#include <uhid/ps5.hpp>
#include <uhid/dualsense_edge.hpp>
#include <uhid/uhid.hpp>

namespace inputtino {

static uint32_t sign_crc32(uint32_t seed, const unsigned char *buffer, size_t length) {
  auto crc = CRC32(buffer, length, seed);
  crc = htole32(crc); // Convert to little endian
  return crc;
}

/**
 * Write one input report. The state is advanced and copied under the state
 * mutex; the (possibly slow) device write happens without it. Called from the
 * report thread and from setters, never with the mutex held.
 */
static void send_report(PS5JoypadState &state) {
  uhid::dualsense_input_report report;
  {
    std::lock_guard lock(state.mutex);
    // setup timestamp and increase seq_number
    state.current_state.seq_number++;
    if (state.current_state.seq_number >= 255) {
      state.current_state.seq_number = 0;
    }

    // Seems that the timestamp is little endian and 0.33us units
    // see:
    // https://github.com/torvalds/linux/blob/305230142ae0637213bf6e04f6d9f10bbcb74af8/drivers/hid/hid-playstation.c#L1409-L1410
    auto now = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::system_clock::now().time_since_epoch())
                   .count();
    state.current_state.sensor_timestamp = htole32(now / 333);
    report = state.current_state;
  }

  struct uhid_event ev{};
  {
    ev.type = UHID_INPUT2;

    std::size_t header_size;
    if (state.is_bluetooth) {
      auto header = uhid::dualsense_input_report_bt_header{};
      header_size = sizeof(header);
      std::copy(reinterpret_cast<unsigned char *>(&header),
                reinterpret_cast<unsigned char *>(&header) + header_size,
                &ev.u.input2.data[0]);
    } else {
      auto header = uhid::dualsense_input_report_usb_header{};
      header_size = sizeof(header);
      std::copy(reinterpret_cast<unsigned char *>(&header),
                reinterpret_cast<unsigned char *>(&header) + header_size,
                &ev.u.input2.data[0]);
    }

    unsigned char *data = (unsigned char *)&report;
    std::copy(data, data + sizeof(report), &ev.u.input2.data[header_size]);

    ev.u.input2.size = header_size + sizeof(report);
  }

  if (state.is_bluetooth) { // CRC32 encode the data and append it to the reply
    ev.u.input2.size += uhid::PS_INPUT_REPORT_BT_OFFSET;

    auto end_of_msg = ev.u.input2.size - 4; // (Last 4 bytes contains crc32)
    auto crc = sign_crc32(uhid::PS_INPUT_CRC32, &ev.u.input2.data[0], end_of_msg);
    std::copy(reinterpret_cast<unsigned char *>(&crc),
              reinterpret_cast<unsigned char *>(&crc) + 4,
              &ev.u.input2.data[end_of_msg]);
  }

  state.dev->send(ev);
}

/**
 * Build the reply payload for a supported feature report into `out`.
 *
 * Returns the payload size, or 0 when the request must be answered with an
 * error: an unsupported report number, or a report type other than feature.
 * Only a valid payload carries the Bluetooth CRC trailer; the size checks
 * below are what make `size - 4` and every copy in-bounds.
 */
static size_t feature_report(const PS5JoypadState &state,
                             uint8_t rnum,
                             uint8_t rtype,
                             unsigned char (&out)[UHID_DATA_MAX]) {
  if (rtype != UHID_FEATURE_REPORT) {
    return 0;
  }
  const unsigned char *table = nullptr;
  size_t size = 0;
  switch (rnum) {
  case uhid::PS5_REPORT_TYPES::CALIBRATION:
    table = uhid::ps5_calibration_info;
    size = sizeof(uhid::ps5_calibration_info);
    break;
  case uhid::PS5_REPORT_TYPES::PAIRING_INFO:
    table = uhid::ps5_pairing_info;
    size = sizeof(uhid::ps5_pairing_info);
    break;
  case uhid::PS5_REPORT_TYPES::FIRMWARE_INFO:
    table = uhid::ps5_firmware_info;
    size = sizeof(uhid::ps5_firmware_info);
    break;
  default:
    return 0;
  }
  static_assert(sizeof(uhid::ps5_calibration_info) <= UHID_DATA_MAX &&
                    sizeof(uhid::ps5_pairing_info) <= UHID_DATA_MAX &&
                    sizeof(uhid::ps5_firmware_info) <= UHID_DATA_MAX,
                "feature replies fit a UHID event");
  std::copy(table, table + size, &out[0]);

  if (rnum == uhid::PS5_REPORT_TYPES::PAIRING_INFO) {
    static_assert(sizeof(uhid::ps5_pairing_info) >= 1 + sizeof(state.mac_address), "pairing reply holds the MAC");
    std::reverse_copy(&state.mac_address[0], &state.mac_address[0] + sizeof(state.mac_address), &out[1]);
  }

  if (state.is_bluetooth) {
    // The last 4 bytes of every Bluetooth feature reply hold its CRC32.
    if (size <= 4) {
      return 0;
    }
    auto end_of_msg = size - 4;
    auto crc = sign_crc32(uhid::PS_FEATURE_CRC32, &out[0], end_of_msg);
    std::copy(reinterpret_cast<unsigned char *>(&crc), reinterpret_cast<unsigned char *>(&crc) + 4, &out[end_of_msg]);
  }
  return size;
}

/**
 * The common output-report block of a UHID_OUTPUT event, or nullopt when the
 * report is of another type or too short to contain it.
 */
static std::optional<uhid::dualsense_output_report_common> output_report(const uhid_event &ev) {
  const auto size = std::min<size_t>(ev.u.output.size, sizeof(ev.u.output.data));
  const auto *data = ev.u.output.data;
  size_t offset = 0;
  if (size >= 1 && data[0] == uhid::DS_OUTPUT_REPORT_USB) {
    offset = offsetof(uhid::dualsense_output_report_usb, common);
  } else if (size >= 2 && data[0] == uhid::DS_OUTPUT_REPORT_BT) {
    offset = offsetof(uhid::dualsense_output_report_bt, common);
    /*
     * SDL2 sets the EnableHID flag and will send the output report straight after
     * https://github.com/libsdl-org/SDL/blob/c8c4c9772758de2ae466d27f13eb3ed4233e3f32/src/joystick/hidapi/SDL_hidapi_ps5.c#L788-L789
     *
     * The Linux kernel instead, sets this as 0, properly set the SeqNo and adds a hardcoded `tag` field before the
     * actual output report
     * https://github.com/torvalds/linux/blob/305230142ae0637213bf6e04f6d9f10bbcb74af8/drivers/hid/hid-playstation.c#L1184-L1192
     */
    auto report_bt = reinterpret_cast<const uhid::dualsense_output_report_bt *>(data);
    if (report_bt->EnableHID == 0) {
      offset += 1; // Skip the tag field
    }
  } else {
    return std::nullopt;
  }
  if (size < offset + sizeof(uhid::dualsense_output_report_common)) {
    return std::nullopt;
  }
  uhid::dualsense_output_report_common report;
  std::memcpy(&report, data + offset, sizeof(report));
  return report;
}

static void on_uhid_event(std::shared_ptr<PS5JoypadState> state, uhid_event ev, int fd) {
  switch (ev.type) {
  case UHID_GET_REPORT: {
    uhid_event answer{};
    answer.type = UHID_GET_REPORT_REPLY;
    answer.u.get_report_reply.id = ev.u.get_report.id;
    // `uhid_event` is packed; build the payload unaligned-safe, then copy it in.
    unsigned char payload[UHID_DATA_MAX] = {};
    auto size = feature_report(*state, ev.u.get_report.rnum, ev.u.get_report.rtype, payload);
    std::memcpy(answer.u.get_report_reply.data, payload, size);
    if (size == 0) {
      // An error reply carries no payload; the kernel reports EIO to the reader.
      answer.u.get_report_reply.err = EIO;
      answer.u.get_report_reply.size = 0;
    } else {
      answer.u.get_report_reply.err = 0;
      answer.u.get_report_reply.size = static_cast<__u16>(size);
    }

    auto res = uhid::uhid_write(fd, &answer);
    // TODO: signal error somehow
    break;
  }
  case UHID_OUTPUT: { // This is sent if the HID device driver wants to send raw data to the device
    // Here is where we'll get Rumble and LED events
    auto parsed = output_report(ev);
    if (!parsed) {
      break;
    }
    const auto &report = *parsed;

    /*
     * RUMBLE
     * The PS5 joypad seems to report values in the range 0-255,
     * we'll turn those into 0-0xFFFF
     */
    // Callbacks are copied under the state mutex and run without it.
    decltype(state->on_rumble) on_rumble;
    decltype(state->on_led) on_led;
    decltype(state->on_trigger_effect) on_trigger_effect;
    {
      std::lock_guard lock(state->mutex);
      on_rumble = state->on_rumble;
      on_led = state->on_led;
      on_trigger_effect = state->on_trigger_effect;
    }
    if (report.valid_flag0 & uhid::MOTOR_OR_COMPATIBLE_VIBRATION || report.valid_flag2 & uhid::COMPATIBLE_VIBRATION) {
      auto left = (report.motor_left / 255.0f) * 0xFFFF;
      auto right = (report.motor_right / 255.0f) * 0xFFFF;
      if (on_rumble) {
        (*on_rumble)(left, right);
      }
    } else if (report.valid_flag0 == 0 && report.valid_flag1 == 0 && report.valid_flag2 == 0) {
      // Seems to be a special stop rumble event, let's propagate it
      if (on_rumble) {
        (*on_rumble)(0, 0);
      }
    }

    /**
     * Trigger effects
     */
    bool right_trigger = report.valid_flag0 & uhid::RIGHT_TRIGGER_EFFECT;
    bool left_trigger = report.valid_flag0 & uhid::LEFT_TRIGGER_EFFECT;
    if ((right_trigger || left_trigger) && on_trigger_effect) {
      auto left_array_start = std::begin(report.left_trigger_effect);
      auto left_array_end = std::end(report.left_trigger_effect);
      auto right_array_start = std::begin(report.right_trigger_effect);
      auto right_array_end = std::end(report.right_trigger_effect);
      // We have to cache these values because these flags will be set as long as the effect is active
      uint32_t left_trigger_hash = std::accumulate(left_array_start, left_array_end, 0ul);
      uint32_t right_trigger_hash = std::accumulate(right_array_start, right_array_end, 0ul);
      bool changed = false;
      {
        std::lock_guard lock(state->mutex);
        changed = (left_trigger && state->last_left_trigger_event != left_trigger_hash) ||
                  (right_trigger && state->last_right_trigger_event != right_trigger_hash);
        // First, update the cache
        if (changed && left_trigger)
          state->last_left_trigger_event = left_trigger_hash;
        if (changed && right_trigger)
          state->last_right_trigger_event = right_trigger_hash;
      }
      if (changed) {
        // Then, trigger the event
        uint8_t event_flags = (report.valid_flag0 & uhid::LEFT_TRIGGER_EFFECT) |
                              (report.valid_flag0 & uhid::RIGHT_TRIGGER_EFFECT);
        PS5Joypad::TriggerEffect effect = {.event_flags = event_flags,
                                           .type_left = report.left_trigger_effect_type,
                                           .type_right = report.right_trigger_effect_type};
        std::copy(left_array_start, left_array_end, std::begin(effect.left));
        std::copy(right_array_start, right_array_end, std::begin(effect.right));
        (*on_trigger_effect)(effect);
      }
    }

    /*
     * LED
     */
    if (report.valid_flag1 & uhid::LIGHTBAR_ENABLE) {
      if (on_led) {
        // TODO: should we blend brightness?
        (*on_led)(report.lightbar_red, report.lightbar_green, report.lightbar_blue);
      }
    }
  }
  default:
    break;
  }
}

PS5Joypad::PS5Joypad(uint16_t vendor_id, std::array<unsigned char, 6> mac_address)
    : _state(std::make_shared<PS5JoypadState>()) {
  std::copy(mac_address.begin(), mac_address.end(), this->_state->mac_address);
  this->_state->vendor_id = vendor_id;
  // Set touchpad as not pressed
  this->_state->current_state.points[0].contact = 1;
  this->_state->current_state.points[1].contact = 1;
  // Set the battery to 100% (so that if the client doesn't report it we don't trigger annoying low battery warnings)
  this->_state->current_state.battery_charge = 10;
  this->_state->current_state.battery_status = BATTERY_FULL;
}

PS5Joypad::~PS5Joypad() {
  if (this->_state) {
    // Owned threads are joined before the device they use disappears: first
    // the report thread, then the UHID event thread (no callback runs after
    // `stop_thread` returns), then the device itself.
    this->_state->stop_repeat_thread = true;
    if (this->_send_input_thread.joinable()) {
      this->_send_input_thread.join();
    }
    if (this->_state->dev) {
      this->_state->dev->stop_thread();
      this->_state->dev.reset(); // Will trigger ~Device and ultimately destroy the device
    }
  }
}

Result<PS5Joypad> PS5Joypad::create(const DeviceDefinition &device) {
  bool use_bluetooth = true; // TODO: expose this

  auto def = uhid::DeviceDefinition{
      .name = device.name,
      .phys = device.device_phys,
      .uniq = device.device_uniq,
      .bus = BUS_BLUETOOTH,
      .vendor = static_cast<uint32_t>(device.vendor_id),
      .product = static_cast<uint32_t>(device.product_id),
      .version = static_cast<uint32_t>(device.version),
      .country = 0,
      .report_description = {&uhid::ps5_rdesc_bt[0], &uhid::ps5_rdesc_bt[0] + sizeof(uhid::ps5_rdesc_bt)}};

  if (!use_bluetooth) {
    def.bus = BUS_USB;
    def.report_description = {&uhid::ps5_rdesc[0], &uhid::ps5_rdesc[0] + sizeof(uhid::ps5_rdesc)};
  }

  std::array<unsigned char, 6> mac_address = {};
  if (def.uniq.empty()) {
    mac_address = generate_mac_address();
  } else {
    // Assuming we have in input a MAC address in the format of xx:xx:xx:xx:xx:xx
    std::stringstream ss(def.uniq);
    for (int i = 0; i < 6; ++i) {
      unsigned int value;
      ss >> std::hex >> value;
      mac_address[i] = static_cast<unsigned char>(value);
      if (i < 5)
        ss.ignore(1, ':');
    }
  }
  auto joypad = PS5Joypad(device.vendor_id, mac_address);
  joypad._state->is_edge = uhid::is_dualsense_edge(device.vendor_id, device.product_id);

  if (def.phys.empty()) {
    def.phys = "INPUTTINO_BT_LINK";
  }
  if (def.uniq.empty()) {
    def.uniq = joypad.get_mac_address();
  }

  // Set before the event thread can serve feature reports.
  joypad._state->is_bluetooth = use_bluetooth;
  auto dev =
      uhid::Device::create(def, [state = joypad._state](uhid_event ev, int fd) { on_uhid_event(state, ev, fd); });
  if (dev) {
    joypad._state->dev = std::make_shared<uhid::Device>(std::move(*dev));

    // Readers will expect frequent events event if the state hasn't changed
    joypad._send_input_thread = std::thread([state = joypad._state]() {
      while (!state->stop_repeat_thread) {
        send_report(*state);
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
      }
    });

    return joypad;
  }
  return Error(dev.getErrorMessage());
}

static int scale_value(int input, int input_start, int input_end, int output_start, int output_end) {
  auto slope = 1.0 * (output_end - output_start) / (input_end - input_start);
  return output_start + std::round(slope * (input - input_start));
}

template <typename T> std::string to_hex(T i) {
  std::stringstream stream;
  stream << std::hex << std::uppercase << i;
  return stream.str();
}

std::string PS5Joypad::get_mac_address() const {
  std::stringstream stream;
  stream << std::hex << std::setfill('0') << std::setw(2) << (unsigned int)_state->mac_address[0] << ":" << std::setw(2)
         << (unsigned int)_state->mac_address[1] << ":" << std::setw(2) << (unsigned int)_state->mac_address[2] << ":"
         << std::setw(2) << (unsigned int)_state->mac_address[3] << ":" << std::setw(2)
         << (unsigned int)_state->mac_address[4] << ":" << std::setw(2) << (unsigned int)_state->mac_address[5];
  return stream.str();
}

/**
 * The trick here is to match the devices under /sys/devices/virtual/misc/uhid/
 * with the MAC address that we've set for the current device
 *
 * @returns a list of paths to the created input devices ex:
 * /sys/devices/virtual/misc/uhid/0003:054C:0CE6.000D/input/input58/
 */
std::vector<std::string> PS5Joypad::get_sys_nodes() const {
  std::vector<std::string> nodes;
  auto base_path = "/sys/devices/virtual/misc/uhid/";
  auto target_mac = get_mac_address();
  if (std::filesystem::exists(base_path)) {
    auto uhid_entries = std::filesystem::directory_iterator{base_path};
    for (auto uhid_entry : uhid_entries) {
      // Here we are looking for a directory that has a name like {BUS_ID}:{VENDOR_ID}:{PRODUCT_ID}.xxxx
      // (ex: 0003:054C:0CE6.000D)
      auto uhid_candidate_path = uhid_entry.path().filename().string();
      auto target_id = to_hex(this->_state->vendor_id);
      if (uhid_entry.is_directory() && uhid_candidate_path.find(target_id) != std::string::npos) {
        // Found a match! Let's scan the input devices in that directory
        if (std::filesystem::exists(uhid_entry.path() / "input")) {
          // ex: /sys/devices/virtual/misc/uhid/0003:054C:0CE6.000D/input/
          auto dev_entries = std::filesystem::directory_iterator{uhid_entry.path() / "input"};
          for (auto dev_entry : dev_entries) {
            // Here we only have a match if the "uniq" file inside contains the same MAC address that we've set
            if (dev_entry.is_directory()) {
              // ex: /sys/devices/virtual/misc/uhid/0003:054C:0CE6.000D/input/input58/uniq
              auto dev_uniq_path = dev_entry.path() / "uniq";
              if (std::filesystem::exists(dev_uniq_path)) {
                std::ifstream dev_uniq_file{dev_uniq_path};
                std::string line;
                std::getline(dev_uniq_file, line);
                if (line == target_mac) {
                  nodes.push_back(dev_entry.path().string());
                }
              } else {
                fprintf(stderr, "Unable to get joypad nodes, path %s does not exist\n", dev_uniq_path.string().c_str());
              }
            }
          }
        } else {
          fprintf(stderr, "Unable to get joypad nodes, path %s does not exist\n", uhid_entry.path().string().c_str());
        }
      }
    }
  } else {
    fprintf(stderr, "Unable to get joypad nodes, path %s does not exist\n", base_path);
  }
  return nodes;
}

std::vector<std::string> PS5Joypad::get_nodes() const {
  std::vector<std::string> nodes;

  auto sys_nodes = get_sys_nodes();
  for (const auto dev_entry : sys_nodes) {
    auto dev_nodes = std::filesystem::directory_iterator{dev_entry};
    for (auto dev_node : dev_nodes) {
      if (dev_node.is_directory() && (dev_node.path().filename().string().rfind("event", 0) == 0 ||
                                      dev_node.path().filename().string().rfind("js", 0) == 0)) {
        nodes.push_back(("/dev/input/" / dev_node.path().filename()).string());
      }
    }
  }

  return nodes;
}

void PS5Joypad::set_pressed_buttons(unsigned int pressed) {
  std::unique_lock lock(this->_state->mutex);
  { // First reset everything to non-pressed
    this->_state->current_state.buttons[0] = 0;
    // Don't reset L2 and R2, these are handled in set_triggers
    this->_state->current_state.buttons[1] &= (uhid::L2 | uhid::R2);
    this->_state->current_state.buttons[2] = 0;
    this->_state->current_state.buttons[3] = 0;
  }
  {
    if (DPAD_UP & pressed) {     // Pressed UP
      if (DPAD_LEFT & pressed) { // NW
        this->_state->current_state.buttons[0] |= uhid::HAT_NW;
      } else if (DPAD_RIGHT & pressed) { // NE
        this->_state->current_state.buttons[0] |= uhid::HAT_NE;
      } else { // N
        this->_state->current_state.buttons[0] |= uhid::HAT_N;
      }
    }

    if (DPAD_DOWN & pressed) {   // Pressed DOWN
      if (DPAD_LEFT & pressed) { // SW
        this->_state->current_state.buttons[0] |= uhid::HAT_SW;
      } else if (DPAD_RIGHT & pressed) { // SE
        this->_state->current_state.buttons[0] |= uhid::HAT_SE;
      } else { // S
        this->_state->current_state.buttons[0] |= uhid::HAT_S;
      }
    }

    if (DPAD_LEFT & pressed) {                              // Pressed LEFT
      if (!(DPAD_UP & pressed) && !(DPAD_DOWN & pressed)) { // Pressed only LEFT
        this->_state->current_state.buttons[0] |= uhid::HAT_W;
      }
    }

    if (DPAD_RIGHT & pressed) {                             // Pressed RIGHT
      if (!(DPAD_UP & pressed) && !(DPAD_DOWN & pressed)) { // Pressed only RIGHT
        this->_state->current_state.buttons[0] |= uhid::HAT_E;
      }
    }

    if (!(DPAD_UP & pressed) && !(DPAD_DOWN & pressed) && !(DPAD_LEFT & pressed) && !(DPAD_RIGHT & pressed)) {
      this->_state->current_state.buttons[0] |= uhid::HAT_NEUTRAL;
    }

    // TODO: L2/R2 ??

    if (X & pressed)
      this->_state->current_state.buttons[0] |= uhid::SQUARE;
    if (Y & pressed)
      this->_state->current_state.buttons[0] |= uhid::TRIANGLE;
    if (A & pressed)
      this->_state->current_state.buttons[0] |= uhid::CROSS;
    if (B & pressed)
      this->_state->current_state.buttons[0] |= uhid::CIRCLE;
    if (LEFT_BUTTON & pressed)
      this->_state->current_state.buttons[1] |= uhid::L1;
    if (RIGHT_BUTTON & pressed)
      this->_state->current_state.buttons[1] |= uhid::R1;
    if (LEFT_STICK & pressed)
      this->_state->current_state.buttons[1] |= uhid::L3;
    if (RIGHT_STICK & pressed)
      this->_state->current_state.buttons[1] |= uhid::R3;
    if (START & pressed)
      this->_state->current_state.buttons[1] |= uhid::OPTIONS;
    if (BACK & pressed)
      this->_state->current_state.buttons[1] |= uhid::CREATE;
    if (TOUCHPAD_FLAG & pressed)
      this->_state->current_state.buttons[2] |= uhid::TOUCHPAD;
    if (HOME & pressed)
      this->_state->current_state.buttons[2] |= uhid::PS_HOME;
    if (MISC_FLAG & pressed)
      this->_state->current_state.buttons[2] |= uhid::MIC_MUTE;
    this->_state->current_state.buttons[2] |= uhid::edge_buttons(pressed, this->_state->is_edge);
  }
  lock.unlock();
  send_report(*this->_state);
}
void PS5Joypad::set_triggers(int16_t left, int16_t right) {
  std::unique_lock lock(this->_state->mutex);
  this->_state->current_state.z = scale_value(left, 0, 255, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);
  this->_state->current_state.rz = scale_value(right, 0, 255, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);

  if (left == 0)
    this->_state->current_state.buttons[1] &= ~uhid::L2;
  else
    this->_state->current_state.buttons[1] |= uhid::L2;

  if (right == 0)
    this->_state->current_state.buttons[1] &= ~uhid::R2;
  else
    this->_state->current_state.buttons[1] |= uhid::R2;

  lock.unlock();
  send_report(*this->_state);
}
void PS5Joypad::set_stick(Joypad::STICK_POSITION stick_type, short x, short y) {
  std::unique_lock lock(this->_state->mutex);
  switch (stick_type) {
  case RS: {
    this->_state->current_state.rx = scale_value(x, -32768, 32767, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);
    this->_state->current_state.ry = scale_value(-y, -32768, 32767, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);
    lock.unlock();
  send_report(*this->_state);
    break;
  }
  case LS: {
    this->_state->current_state.x = scale_value(x, -32768, 32767, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);
    this->_state->current_state.y = scale_value(-y, -32768, 32767, uhid::PS5_AXIS_MIN, uhid::PS5_AXIS_MAX);
    lock.unlock();
  send_report(*this->_state);
    break;
  }
  }
}
void PS5Joypad::set_on_rumble(const std::function<void(int, int)> &callback) {
  std::lock_guard lock(this->_state->mutex);
  this->_state->on_rumble = callback;
}

/**
 * For a rationale behind this, see: https://github.com/LizardByte/Sunshine/issues/3247#issuecomment-2428065349
 */
static __le16 to_le_signed(float original, float value) {
  value = std::clamp(value, static_cast<float>(SHRT_MIN), static_cast<float>(SHRT_MAX));
  return htole16(value);
}

void PS5Joypad::set_motion(PS5Joypad::MOTION_TYPE type, float x, float y, float z) {
  std::unique_lock lock(this->_state->mutex);
  switch (type) {
  case ACCELERATION: {
    this->_state->current_state.accel[0] = to_le_signed(x, (x * uhid::SDL_STANDARD_GRAVITY_CONST * 100));
    this->_state->current_state.accel[1] = to_le_signed(y, (y * uhid::SDL_STANDARD_GRAVITY_CONST * 100));
    this->_state->current_state.accel[2] = to_le_signed(z, (z * uhid::SDL_STANDARD_GRAVITY_CONST * 100));

    lock.unlock();
  send_report(*this->_state);
    break;
  }
  case GYROSCOPE: {
    this->_state->current_state.gyro[0] = to_le_signed(x, x * uhid::gyro_resolution);
    this->_state->current_state.gyro[1] = to_le_signed(y, y * uhid::gyro_resolution);
    this->_state->current_state.gyro[2] = to_le_signed(z, z * uhid::gyro_resolution);

    lock.unlock();
  send_report(*this->_state);
    break;
  }
  }
}

void PS5Joypad::set_battery(PS5Joypad::BATTERY_STATE state, int percentage) {
  std::unique_lock lock(this->_state->mutex);
  /*
   * Each unit of battery data corresponds to 10%
   * 0 = 0-9%, 1 = 10-19%, .. and 10 = 100%
   */
  this->_state->current_state.battery_charge = std::lround((percentage / 10));
  this->_state->current_state.battery_status = state;
  lock.unlock();
  send_report(*this->_state);
}

void PS5Joypad::set_on_led(const std::function<void(int, int, int)> &callback) {
  std::lock_guard lock(this->_state->mutex);
  this->_state->on_led = callback;
}

void PS5Joypad::set_on_trigger_effect(const std::function<void(const TriggerEffect &)> &callback) {
  std::lock_guard lock(this->_state->mutex);
  this->_state->on_trigger_effect = callback;
}

void PS5Joypad::place_finger(int finger_nr, uint16_t x, uint16_t y) {
  std::unique_lock lock(this->_state->mutex);
  if (finger_nr <= 1) {
    // If this finger was previously unpressed, we should increase the touch id
    if (this->_state->current_state.points[finger_nr].contact == 1) {
      this->_state->current_state.points[finger_nr].id = ++this->_state->last_touch_id;
    }
    this->_state->current_state.points[finger_nr].contact = 0;

    this->_state->current_state.points[finger_nr].x_lo = static_cast<uint8_t>(x & 0x00FF);
    this->_state->current_state.points[finger_nr].x_hi = static_cast<uint8_t>((x & 0x0F00) >> 8);

    this->_state->current_state.points[finger_nr].y_lo = static_cast<uint8_t>(y & 0x000F);
    this->_state->current_state.points[finger_nr].y_hi = static_cast<uint8_t>((y & 0x0FF0) >> 4);

    lock.unlock();
  send_report(*this->_state);
  }
}

void PS5Joypad::release_finger(int finger_nr) {
  std::unique_lock lock(this->_state->mutex);
  if (finger_nr <= 1) {
    // if it goes above 0x7F we should reset it to 0
    if (this->_state->last_touch_id >= 0x7E) {
      this->_state->last_touch_id = 0;
    }
    this->_state->current_state.points[finger_nr].contact = 1;
    lock.unlock();
  send_report(*this->_state);
  }
}

} // namespace inputtino
