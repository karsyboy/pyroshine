#ifdef NDEBUG
#undef NDEBUG
#endif
#include <inputtino/xbox_elite.hpp>
#include <cassert>
#include <vector>
#include <utility>

int main() {
  using namespace inputtino;
  for (auto product : {0x02dd, 0x02e3, 0x0b00}) {
    DeviceDefinition device{"Elite capability test", 0x045e, (uint16_t)product, 0x100};
    auto dev = xbox_evdev_definition(device);
    assert(dev);
    assert(libevdev_get_id_vendor(dev) == 0x045e);
    assert(libevdev_get_id_product(dev) == product);
    for (auto key : elite_keys)
      assert(libevdev_has_event_code(dev, EV_KEY, key) == (product != 0x02dd));
    assert(libevdev_has_event_code(dev, EV_KEY, BTN_SOUTH));
    assert(libevdev_has_event_code(dev, EV_ABS, ABS_Z));
    assert(libevdev_has_event_code(dev, EV_FF, FF_RUMBLE));
    libevdev_free(dev);
  }
  assert(!is_xbox_elite(0x28de, 0x0b00));
  // Exhaust every state transition, including full neutralization and reuse.
  for (unsigned previous = 0; previous < 16; ++previous) {
    for (unsigned pressed = 0; pressed < 16; ++pressed) {
      std::vector<std::pair<unsigned, int>> events;
      emit_elite_changes(pressed << 16, previous << 16, true,
                        [&](unsigned key, int value) { events.emplace_back(key, value); });
      unsigned event = 0;
      for (unsigned i = 0; i < 4; ++i) {
        if ((pressed ^ previous) & (1 << i)) {
          assert(events[event].first == elite_keys[i]);
          assert(events[event++].second == ((pressed >> i) & 1));
        }
      }
      assert(event == events.size());
      emit_elite_changes((pressed << 16) | Joypad::A, (pressed << 16), true,
                        [](unsigned, int) { assert(false); });
      emit_elite_changes(pressed << 16, previous << 16, false,
                        [](unsigned, int) { assert(false); });
    }
  }
}
