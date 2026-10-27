// Copyright (c) 2026
// SPDX-License-Identifier: MIT
#pragma once
#include <cstddef>
#include <cstdint>

namespace PyroWave
{
// Validate every byte consumed by the GPU coefficient unpacker before upload.
// Uses byte loads, so network packet alignment is irrelevant.
inline bool validate_coefficient_packet(const uint8_t *data, size_t size)
{
	if (!data || size < 8 || (size & 3))
		return false;
	unsigned ballot = unsigned(data[0]) | (unsigned(data[1]) << 8);
	unsigned words = (unsigned(data[2]) | (unsigned(data[3]) << 8)) & 0xfff;
	if (size != words * 4 || words < 2)
		return false;
	unsigned count = 0;
	for (unsigned bits = ballot; bits; bits &= bits - 1)
		count++;
	size_t offset = 8 + count * 3;
	if (offset > size)
		return false;
	unsigned significant = 0;
	for (unsigned i = 0; i < count; i++)
	{
		unsigned control = unsigned(data[8 + i * 2]) | (unsigned(data[9 + i * 2]) << 8);
		unsigned q = data[8 + count * 2 + i] & 15;
		for (unsigned sub = 0; sub < 8; sub++)
		{
			unsigned planes = q + ((control >> (sub * 2)) & 3);
			if (planes > size - offset)
				return false;
			unsigned mask = 0;
			for (unsigned p = 0; p < planes; p++)
				mask |= data[offset++];
			for (; mask; mask &= mask - 1)
				significant++;
		}
	}
	return ((offset + (significant + 7) / 8 + 3) & ~size_t(3)) == size;
}
}
