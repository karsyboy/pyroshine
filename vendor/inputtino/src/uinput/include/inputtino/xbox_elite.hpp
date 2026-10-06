#pragma once
#include <cstdint>
#include <inputtino/input.hpp>
#include <libevdev/libevdev.h>
#include <linux/input.h>

// These codes were added after older distro headers. Their Linux UAPI values
// are stable (input-event-codes.h); do not substitute BTN_TRIGGER_HAPPY.
#ifndef BTN_GRIPL
#define BTN_GRIPL 0x224
#define BTN_GRIPR 0x225
#define BTN_GRIPL2 0x226
#define BTN_GRIPR2 0x227
#endif

namespace inputtino {
static_assert(Joypad::PADDLE1_FLAG == 0x10000 && Joypad::PADDLE2_FLAG == 0x20000 &&
              Joypad::PADDLE3_FLAG == 0x40000 && Joypad::PADDLE4_FLAG == 0x80000,
              "Inputtino must retain the Moonlight paddle flags");
constexpr bool is_xbox_elite(uint16_t vendor, uint16_t product) {
  return vendor == 0x045e && (product == 0x02e3 || product == 0x0b00);
}
// SDL canonical order: upper right, upper left, lower right, lower left.
constexpr unsigned int elite_flags[] = {
    Joypad::PADDLE1_FLAG, Joypad::PADDLE2_FLAG, Joypad::PADDLE3_FLAG, Joypad::PADDLE4_FLAG};
constexpr unsigned int elite_keys[] = {BTN_GRIPR, BTN_GRIPL, BTN_GRIPR2, BTN_GRIPL2};

template <typename Emit>
void emit_elite_changes(unsigned int pressed, unsigned int previous, bool elite, Emit emit) {
  if (!elite) return;
  for (unsigned int i = 0; i < 4; ++i) {
    if ((pressed ^ previous) & elite_flags[i])
      emit(elite_keys[i], (pressed & elite_flags[i]) ? 1 : 0);
  }
}

// Owned by the caller; separated from uinput creation for capability tests.
libevdev *xbox_evdev_definition(const DeviceDefinition &device);
}
