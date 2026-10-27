#include "pyrowave_packet_validation.hpp"
#include <cassert>
#include <cstdio>
#include <vector>
int main() {
    uint8_t empty[8] = {0, 0, 2, 0, 0, 0, 0, 0};
    assert(PyroWave::validate_coefficient_packet(empty, sizeof(empty)));
    for (size_t n = 0; n < 8; n++) assert(!PyroWave::validate_coefficient_packet(empty, n));
    uint32_t rng = 1;
    for (unsigned iteration = 0; iteration < 200000; iteration++) {
        rng = rng * 1664525u + 1013904223u;
        std::vector<uint8_t> bytes(rng % 512 + 1);
        for (auto &b : bytes) { rng = rng * 1664525u + 1013904223u; b = rng >> 24; }
        // Frequently match the length header to exercise deeper variable-size parsing.
        if (bytes.size() >= 8 && !(bytes.size() & 3)) {
            bytes[2] = uint8_t(bytes.size() / 4); bytes[3] &= 0xf0;
        }
        PyroWave::validate_coefficient_packet(bytes.data(), bytes.size());
        PyroWave::validate_coefficient_packet(bytes.data() + 1, bytes.size() - 1);
    }
    puts("200000 malformed/alignment packet cases passed");
}
