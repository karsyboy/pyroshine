// Copyright (c) 2026
// SPDX-License-Identifier: MIT
#pragma once
#include <stdint.h>
namespace PyroWave
{
struct BitstreamPacket
{
	uint32_t offset_u32;
	uint32_t num_words;
};

struct BitstreamHeader
{
	uint16_t ballot;
	uint16_t payload_words : 12;
	uint16_t sequence : 3;
	uint16_t extended : 1;
	uint32_t quant_code : 8;
	uint32_t block_index : 24;
};

static_assert(sizeof(BitstreamHeader) == 8, "BitstreamHeader is not 8 bytes.");

enum
{
	BITSTREAM_EXTENDED_CODE_START_OF_FRAME = 0,
};

enum
{
	CHROMA_RESOLUTION_420 = 0,
	CHROMA_RESOLUTION_444 = 1
};

enum
{
	CHROMA_SITING_CENTER = 0,
	CHROMA_SITING_LEFT = 1
};

enum
{
	YCBCR_RANGE_FULL = 0,
	YCBCR_RANGE_LIMITED = 1
};

enum
{
	COLOR_PRIMARIES_SRGB = 0,
	COLOR_PRIMARIES_BT2020 = 1
};

enum
{
	YCBCR_TRANSFORM_BT709 = 0,
	YCBCR_TRANSFORM_BT2020 = 1
};

enum
{
	TRANSFER_FUNCTION_SRGB = 0,
	TRANSFER_FUNCTION_PQ = 1
};

static constexpr uint32_t SequenceCountMask = 0x7;

struct BitstreamSequenceHeader
{
	uint32_t width_minus_1 : 14;
	uint32_t height_minus_1 : 14;
	uint32_t sequence : 3;
	uint32_t extended : 1;
	uint32_t total_blocks : 24;
	uint32_t code : 2;
	uint32_t chroma_resolution : 1;
	uint32_t color_primaries : 1;
	uint32_t transfer_function : 1;
	uint32_t ycbcr_transform : 1;
	uint32_t ycbcr_range : 1;
	uint32_t chroma_siting : 1;
};

static_assert(sizeof(BitstreamSequenceHeader) == 8, "BitstreamSequenceHeader is not 8 bytes.");

}
