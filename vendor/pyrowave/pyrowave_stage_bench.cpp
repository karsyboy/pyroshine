// Copyright (c) 2026 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT

// Encoder stage benchmark and output fingerprint for shader work.
//
//   pyrowave-stage-bench <frames.rgba> <width> <height> [444|420] [sdr|hdr] [iterations] [budget-bytes]
//
// Loads tightly packed RGBA8 frames, encodes them repeatedly through the C
// API scaled path (as Pyroshine does), prints the per-stage GPU timestamps,
// then decodes each frame once and prints an FNV-1a hash of the decoded R16
// planes. Optimizations must leave every hash unchanged; bitstream bytes are
// not compared because payload placement is atomic-allocated.

#include "device.hpp"
#include "context.hpp"
#include "image.hpp"

#include "pyrowave.h"
#include <stdio.h>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>
#include <vector>

using namespace Vulkan;

#define ASSERT_THAT(x) do { \
	if (!(x)) { fprintf(stderr, "Fatal error executing %s at line %d.\n", #x, __LINE__); std::terminate(); } \
} while(false)

#define CHECKED(x) do { \
	pyrowave_result _res = x; \
	if (_res != PYROWAVE_SUCCESS) { fprintf(stderr, "Got pyrowave result %d while executing %s at line %d.\n", _res, #x, __LINE__); std::terminate(); } \
} while(false)

int main(int argc, char **argv)
{
	if (argc < 4)
	{
		fprintf(stderr, "usage: %s frames.rgba width height [444|420] [sdr|hdr] [iterations] [budget-bytes]\n", argv[0]);
		return 2;
	}
	const int width = atoi(argv[2]), height = atoi(argv[3]);
	const bool c444 = argc <= 4 || strcmp(argv[4], "420") != 0;
	const bool hdr = argc > 5 && strcmp(argv[5], "hdr") == 0;
	const int iterations = argc > 6 ? atoi(argv[6]) : 600;
	const size_t budget = argc > 7 ? size_t(atoll(argv[7])) : 416664;

	FILE *file = fopen(argv[1], "rb");
	ASSERT_THAT(file);
	const size_t frame_bytes = size_t(width) * height * 4;
	std::vector<std::vector<uint8_t>> frames;
	for (;;)
	{
		std::vector<uint8_t> frame(frame_bytes);
		if (fread(frame.data(), 1, frame_bytes, file) != frame_bytes)
			break;
		for (size_t i = 3; i < frame_bytes; i += 4)
			frame[i] = 0xff;
		frames.push_back(std::move(frame));
	}
	fclose(file);
	ASSERT_THAT(!frames.empty());

	ASSERT_THAT(Context::init_loader(nullptr));
	Context ctx;
	ctx.set_num_thread_indices(1);
	ctx.set_system_handles({});
	VkApplicationInfo app_info = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
	app_info.apiVersion = VK_API_VERSION_1_3;
	app_info.pApplicationName = "pyrowave-stage-bench";
	app_info.pEngineName = "Granite";
	ctx.set_application_info(&app_info);
	ASSERT_THAT(ctx.init_instance_and_device(nullptr, 0, nullptr, 0));
	Device device;
	device.set_context(ctx);

	VkInstanceCreateInfo instance_create_info = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
	instance_create_info.enabledExtensionCount = device.get_device_features().num_instance_extensions;
	instance_create_info.ppEnabledExtensionNames = device.get_device_features().instance_extensions;
	instance_create_info.pApplicationInfo = &ctx.get_application_info();
	VkDeviceCreateInfo device_create_info = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO };
	// Both families are handed over; the encoder then runs on compute so a
	// hang from experimental shaders resets only a compute ring, never the
	// desktop's graphics ring (amdgpu resets just the guilty ring).
	const auto &qi = device.get_queue_info();
	VkDeviceQueueCreateInfo queue_infos[2] = {};
	pyrowave_device_create_queue_info device_queue_infos[2] = {};
	const QueueIndices indices[2] = { QUEUE_INDEX_GRAPHICS, QUEUE_INDEX_COMPUTE };
	uint32_t queue_count = qi.family_indices[QUEUE_INDEX_COMPUTE] != qi.family_indices[QUEUE_INDEX_GRAPHICS] ? 2 : 1;
	static const float priority = 1.0f;
	for (uint32_t i = 0; i < queue_count; i++)
	{
		queue_infos[i] = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO };
		queue_infos[i].queueFamilyIndex = qi.family_indices[indices[i]];
		queue_infos[i].queueCount = 1;
		queue_infos[i].pQueuePriorities = &priority;
		device_queue_infos[i].familyIndex = qi.family_indices[indices[i]];
		device_queue_infos[i].index = 0;
		device_queue_infos[i].queue = qi.queues[indices[i]];
	}
	device_create_info.pNext = ctx.get_enabled_device_features().pdf2;
	device_create_info.enabledExtensionCount = ctx.get_enabled_device_features().num_device_extensions;
	device_create_info.ppEnabledExtensionNames = ctx.get_enabled_device_features().device_extensions;
	device_create_info.queueCreateInfoCount = queue_count;
	device_create_info.pQueueCreateInfos = queue_infos;
	pyrowave_device_create_info info = {};
	info.GetInstanceProcAddr = vkGetInstanceProcAddr;
	info.instance = ctx.get_instance();
	info.physical_device = ctx.get_gpu();
	info.device = ctx.get_device();
	info.device_create_info = &device_create_info;
	info.instance_create_info = &instance_create_info;
	info.queue_info_count = queue_count;
	info.queue_info = device_queue_infos;
	info.userdata = &device;
	info.queue_lock_callback = [](void *userdata) { static_cast<Device *>(userdata)->external_queue_lock(); };
	info.queue_unlock_callback = [](void *userdata) { static_cast<Device *>(userdata)->external_queue_unlock(); };
	pyrowave_device pyro = nullptr;
	CHECKED(pyrowave_create_device(&info, &pyro));
	if (!getenv("PYROWAVE_STAGE_BENCH_GRAPHICS"))
		CHECKED(pyrowave_device_set_queue_type(pyro, VK_QUEUE_COMPUTE_BIT));

	pyrowave_encoder encoder = nullptr;
	pyrowave_encoder_create_info encoder_info = {};
	encoder_info.device = pyro;
	encoder_info.chroma = c444 ? PYROWAVE_CHROMA_SUBSAMPLING_444 : PYROWAVE_CHROMA_SUBSAMPLING_420;
	encoder_info.width = width;
	encoder_info.height = height;
	CHECKED(pyrowave_encoder_create(&encoder_info, &encoder));

	std::vector<ImageHandle> inputs;
	for (auto &frame : frames)
	{
		auto image_info = ImageCreateInfo::immutable_2d_image(width, height, VK_FORMAT_R8G8B8A8_UNORM);
		image_info.initial_layout = VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL;
		ImageInitialData initial = { frame.data() };
		inputs.push_back(device.create_image(image_info, &initial));
		ASSERT_THAT(inputs.back());
	}
	device.wait_idle();

	auto scaling_for = [&](const ImageHandle &input) {
		pyrowave_scaled_encode_info scaling = {};
		scaling.view.image = input->get_image();
		scaling.view.width = width;
		scaling.view.height = height;
		scaling.view.image_format = scaling.view.view_format = VK_FORMAT_R8G8B8A8_UNORM;
		scaling.view.layout = input->get_layout(VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL);
		scaling.view.aspect = VK_IMAGE_ASPECT_COLOR_BIT;
		scaling.intermediate_plane_format = hdr ? VK_FORMAT_R16_UNORM : VK_FORMAT_R8_UNORM;
		scaling.input_color_space = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
		scaling.output_color_space = hdr ? VK_COLOR_SPACE_HDR10_ST2084_EXT : VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
		scaling.ycbcr_chroma_midpoint = hdr ? 512.0f / 1023.0f : 128.0f / 255.0f;
		return scaling;
	};
	pyrowave_rate_control rate_control = { budget };
	std::vector<uint8_t> bitstream(budget + 65536);

	auto encode_one = [&](const ImageHandle &input) -> size_t {
		auto scaling = scaling_for(input);
		CHECKED(pyrowave_encoder_encode_gpu_scaled(encoder, nullptr, nullptr, &scaling, &rate_control));
		size_t num_packets = 0;
		CHECKED(pyrowave_encoder_compute_num_packets(encoder, bitstream.size(), &num_packets));
		// Diagnostic shader variants may produce invalid streams; only time them.
		if (getenv("PYROWAVE_STAGE_BENCH_NO_DECODE"))
			return 0;
		ASSERT_THAT(num_packets == 1);
		pyrowave_packet packet = {};
		CHECKED(pyrowave_encoder_packetize(encoder, &packet, bitstream.size(), &num_packets,
		                                   bitstream.data(), bitstream.size()));
		return packet.size;
	};

	// Warm up, discard timings, then measure.
	for (int i = 0; i < 60; i++)
		encode_one(inputs[i % inputs.size()]);
	pyrowave_device_report_performance_stats(pyro, [](void *, const char *) {}, nullptr, true);
	size_t total_bytes = 0;
	for (int i = 0; i < iterations; i++)
		total_bytes += encode_one(inputs[i % inputs.size()]);
	printf("%dx%d %s %s budget %zu: %d encodes, %.0f bytes/frame\n", width, height, c444 ? "444" : "420",
	       hdr ? "hdr" : "sdr", budget, iterations, double(total_bytes) / iterations);
	pyrowave_device_report_performance_stats(pyro, [](void *, const char *msg) {
		if (!strstr(msg, "Memory Heap"))
			printf("  %s\n", msg);
	}, nullptr, true);
	fflush(stdout);
	if (getenv("PYROWAVE_STAGE_BENCH_NO_DECODE"))
		return 0;

	// Decoded fingerprints.
	pyrowave_decoder decoder = nullptr;
	pyrowave_decoder_create_info decoder_info = {};
	decoder_info.device = pyro;
	decoder_info.chroma = encoder_info.chroma;
	decoder_info.width = width;
	decoder_info.height = height;
	CHECKED(pyrowave_decoder_create(&decoder_info, &decoder));
	ImageHandle outputs[3];
	BufferHandle readbacks[3];
	pyrowave_gpu_buffers buffers = {};
	for (int c = 0; c < 3; c++)
	{
		auto output_info = ImageCreateInfo::immutable_2d_image(c && !c444 ? width / 2 : width,
		                                                       c && !c444 ? height / 2 : height, VK_FORMAT_R16_UNORM);
		output_info.usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_STORAGE_BIT;
		output_info.initial_layout = VK_IMAGE_LAYOUT_GENERAL;
		outputs[c] = device.create_image(output_info);
		auto &plane = buffers.planes[c];
		plane.image = outputs[c]->get_image();
		plane.width = outputs[c]->get_width();
		plane.height = outputs[c]->get_height();
		plane.image_format = plane.view_format = VK_FORMAT_R16_UNORM;
		plane.aspect = VK_IMAGE_ASPECT_COLOR_BIT;
		plane.layout = VK_IMAGE_LAYOUT_GENERAL;
		BufferCreateInfo b = {};
		b.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
		b.size = size_t(plane.width) * plane.height * 2;
		b.domain = BufferDomain::CachedHost;
		readbacks[c] = device.create_buffer(b);
	}
	for (size_t f = 0; f < inputs.size(); f++)
	{
		size_t size = encode_one(inputs[f]);
		device.wait_idle();
		pyrowave_decoder_clear(decoder);
		CHECKED(pyrowave_decoder_push_packet(decoder, bitstream.data(), size));
		ASSERT_THAT(pyrowave_decoder_decode_is_ready(decoder, false));
		auto cmd = device.request_command_buffer();
		pyrowave_device_set_command_buffer(pyro, cmd->get_command_buffer());
		CHECKED(pyrowave_decoder_decode_gpu_buffer(decoder, nullptr, nullptr, &buffers));
		pyrowave_device_set_command_buffer(pyro, VK_NULL_HANDLE);
		for (int c = 0; c < 3; c++)
		{
			cmd->image_barrier(*outputs[c], VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
			                   VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
			                   VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_READ_BIT);
			cmd->copy_image_to_buffer(*readbacks[c], *outputs[c], 0, {},
			                          { outputs[c]->get_width(), outputs[c]->get_height(), 1 }, 0, 0,
			                          { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 });
			cmd->image_barrier(*outputs[c], VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
			                   VK_PIPELINE_STAGE_2_COPY_BIT, 0,
			                   VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);
		}
		cmd->barrier(VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT,
		             VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_2_HOST_READ_BIT);
		Fence fence;
		device.submit(cmd, &fence);
		fence->wait();
		uint64_t hash = 1469598103934665603ull;
		for (int c = 0; c < 3; c++)
		{
			auto *data = static_cast<const uint8_t *>(device.map_host_buffer(*readbacks[c], MEMORY_ACCESS_READ_BIT));
			for (size_t i = 0; i < readbacks[c]->get_create_info().size; i++)
				hash = (hash ^ data[i]) * 1099511628211ull;
		}
		printf("frame %zu: %zu bytes, decoded fnv1a %016llx\n", f, size, (unsigned long long)hash);
	}
	pyrowave_decoder_destroy(decoder);
	pyrowave_encoder_destroy(encoder);
	pyrowave_device_destroy(pyro);
}
