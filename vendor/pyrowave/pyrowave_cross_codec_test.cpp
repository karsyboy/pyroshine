// Copyright (c) 2026 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT

#include "device.hpp"
#include "context.hpp"
#include "image.hpp"
#include "math.hpp"
#include "muglm/muglm_impl.hpp"

#include "pyrowave.h"
#include <stdio.h>
#include <cstdlib>
#include <exception>
#include <vector>
#include <cmath>
#include <algorithm>
#include <cstdint>
#include <cstring>
#include <utility>
#include <initializer_list>

#include "shaders/slangmosh_test.hpp"


using namespace Vulkan;

#define ASSERT_THAT(x) do { \
	if (!(x)) { fprintf(stderr, "Fatal error executing %s at line %d.\n", #x, __LINE__); std::terminate(); } \
} while(false)

#define CHECKED(x) do { \
	pyrowave_result _res = x; \
	if (_res != PYROWAVE_SUCCESS) { fprintf(stderr, "Got pyrowave result %d while executing %s at line %d.\n", _res, #x, __LINE__); std::terminate(); } \
} while(false)

#include <dlfcn.h>
#include <fstream>
#include <string>
static void* encodeLibrary;
static void* decodeLibrary;
static pyrowave_device decodeDevice;
static std::string fixtureRoot;
static bool reuseFixture;
static size_t frameBudget;
template<class T> static T symbol(void* lib, const char* name) {
    auto ptr = reinterpret_cast<T>(dlsym(lib, name));
    if (!ptr) { fprintf(stderr, "Missing symbol %s\n", name); std::abort(); }
    return ptr;
}
#define pyrowave_decoder_create(...) symbol<decltype(&::pyrowave_decoder_create)>(decodeLibrary, "pyrowave_decoder_create")(__VA_ARGS__)
#define pyrowave_decoder_decode_gpu_buffer(...) symbol<decltype(&::pyrowave_decoder_decode_gpu_buffer)>(decodeLibrary, "pyrowave_decoder_decode_gpu_buffer")(__VA_ARGS__)
#define pyrowave_decoder_decode_is_ready(...) symbol<decltype(&::pyrowave_decoder_decode_is_ready)>(decodeLibrary, "pyrowave_decoder_decode_is_ready")(__VA_ARGS__)
#define pyrowave_decoder_push_packet(...) symbol<decltype(&::pyrowave_decoder_push_packet)>(decodeLibrary, "pyrowave_decoder_push_packet")(__VA_ARGS__)
#define pyrowave_encoder_compute_num_packets(...) symbol<decltype(&::pyrowave_encoder_compute_num_packets)>(encodeLibrary, "pyrowave_encoder_compute_num_packets")(__VA_ARGS__)
#define pyrowave_encoder_create(...) symbol<decltype(&::pyrowave_encoder_create)>(encodeLibrary, "pyrowave_encoder_create")(__VA_ARGS__)
// API 1.0 dropped the _synchronous suffix; pre-1.0 reference libraries still export it.
static decltype(&::pyrowave_encoder_encode_gpu_scaled) encode_scaled_symbol(void* lib) {
    auto ptr = reinterpret_cast<decltype(&::pyrowave_encoder_encode_gpu_scaled)>(dlsym(lib, "pyrowave_encoder_encode_gpu_scaled"));
    return ptr ? ptr : symbol<decltype(&::pyrowave_encoder_encode_gpu_scaled)>(lib, "pyrowave_encoder_encode_gpu_scaled_synchronous");
}
#define pyrowave_encoder_encode_gpu_scaled(...) encode_scaled_symbol(encodeLibrary)(__VA_ARGS__)
#define pyrowave_encoder_packetize(...) symbol<decltype(&::pyrowave_encoder_packetize)>(encodeLibrary, "pyrowave_encoder_packetize")(__VA_ARGS__)
#define pyrowave_decoder_destroy(...) symbol<decltype(&::pyrowave_decoder_destroy)>(decodeLibrary, "pyrowave_decoder_destroy")(__VA_ARGS__)
#define pyrowave_encoder_destroy(...) symbol<decltype(&::pyrowave_encoder_destroy)>(encodeLibrary, "pyrowave_encoder_destroy")(__VA_ARGS__)
#define pyrowave_device_destroy(...) symbol<decltype(&::pyrowave_device_destroy)>(encodeLibrary, "pyrowave_device_destroy")(__VA_ARGS__)
static void test_color_pipeline(bool hdr, bool ten, bool c444, int width, int height, int source_space = -1)
{
    if (source_space < 0) source_space = hdr ? 1 : 0;
	ASSERT_THAT(Context::init_loader(nullptr));

	Context ctx;
	ctx.set_num_thread_indices(1);
	ctx.set_system_handles({});

	VkApplicationInfo app_info = { VK_STRUCTURE_TYPE_APPLICATION_INFO };
	app_info.apiVersion = VK_API_VERSION_1_3;
	app_info.pApplicationName = "pyrowave-c-test";
	app_info.pEngineName = "Granite";
	ctx.set_application_info(&app_info);

	ASSERT_THAT(ctx.init_instance_and_device(nullptr, 0, nullptr, 0));

	Device device;
	device.set_context(ctx);

	// Fill in a proxy instance create info.
	VkInstanceCreateInfo instance_create_info = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO };
	instance_create_info.enabledExtensionCount = device.get_device_features().num_instance_extensions;
	instance_create_info.ppEnabledExtensionNames = device.get_device_features().instance_extensions;
	instance_create_info.pApplicationInfo = &ctx.get_application_info();

	// Fill in a proxy device create info.
	VkDeviceCreateInfo device_create_info = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO };
	VkDeviceQueueCreateInfo queue_info = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO };
	queue_info.queueFamilyIndex = device.get_queue_info().family_indices[QUEUE_INDEX_GRAPHICS];
	queue_info.queueCount = 1;
	device_create_info.pNext = ctx.get_enabled_device_features().pdf2;
	device_create_info.enabledExtensionCount = ctx.get_enabled_device_features().num_device_extensions;
	device_create_info.ppEnabledExtensionNames = ctx.get_enabled_device_features().device_extensions;
	device_create_info.queueCreateInfoCount = 1;
	device_create_info.pQueueCreateInfos = &queue_info;

	// Hand over a concrete VkQueue we want implementation to use.
	pyrowave_device_create_queue_info device_queue_info = {};
	device_queue_info.familyIndex = queue_info.queueFamilyIndex;
	device_queue_info.index = 0;
	device_queue_info.queue = device.get_queue_info().queues[QUEUE_INDEX_GRAPHICS];

	pyrowave_device_create_info info = {};
	info.GetInstanceProcAddr = vkGetInstanceProcAddr;
	info.instance = ctx.get_instance();
	info.physical_device = ctx.get_gpu();
	info.device = ctx.get_device();
	info.device_create_info = &device_create_info;
	info.instance_create_info = &instance_create_info;
	info.queue_info_count = 1;
	info.queue_info = &device_queue_info;
	info.userdata = &device;
	info.queue_lock_callback = [](void *userdata) { static_cast<Device *>(userdata)->external_queue_lock(); };
	info.queue_unlock_callback = [](void *userdata) { static_cast<Device *>(userdata)->external_queue_unlock(); };

	pyrowave_encoder encoder;
	pyrowave_decoder decoder;
	pyrowave_device pyro_device;
	CHECKED(symbol<decltype(&pyrowave_create_device)>(encodeLibrary, "pyrowave_create_device")(&info, &pyro_device));
    CHECKED(symbol<decltype(&pyrowave_create_device)>(decodeLibrary, "pyrowave_create_device")(&info, &decodeDevice));

	pyrowave_encoder_create_info encoder_info = {};
	encoder_info.device = pyro_device;
	encoder_info.chroma = c444 ? PYROWAVE_CHROMA_SUBSAMPLING_444 : PYROWAVE_CHROMA_SUBSAMPLING_420;
	encoder_info.width = width;
	encoder_info.height = height;
	CHECKED(pyrowave_encoder_create(&encoder_info, &encoder));

	pyrowave_decoder_create_info decoder_info = {};
	decoder_info.device = decodeDevice;
	decoder_info.chroma = encoder_info.chroma;
	decoder_info.width = width;
	decoder_info.height = height;
	CHECKED(pyrowave_decoder_create(&decoder_info, &decoder));


    // One-pixel chroma edges, grayscale and RGB ramps. Source representation
    // is 10-bit packed UNORM for HDR and ten-bit SDR, eight-bit otherwise.
    const unsigned max = ten ? 1023 : 255;
    auto rgb_at = [=](int x, int y, int channel) -> float {
        if (y < height / 3) {
            // Every texel differs; 4:4:4 must retain the chroma alternation.
            return (channel == 1 ? 0.4f : ((x & 1) == channel ? 0.8f : 0.2f));
        }
        if (y < 2 * height / 3) return float(x) / (width - 1);
        return float((x * (channel + 1) + y * (channel + 2)) % (max + 1)) / max;
    };
    std::vector<uint32_t> source(size_t(width) * height);
    for (int y = 0; y < height; y++) for (int x = 0; x < width; x++) {
        unsigned rgb[3];
        for (int c = 0; c < 3; c++) rgb[c] = unsigned(std::round(rgb_at(x,y,c) * max));
        source[size_t(y) * width + x] = ten ? rgb[0] | (rgb[1] << 10) | (rgb[2] << 20) | (3u << 30)
                                          : rgb[0] | (rgb[1] << 8) | (rgb[2] << 16) | (255u << 24);
    }
    std::vector<uint16_t> linear_source(source.size()*4);
    if (source_space == 2) for (int y=0;y<height;y++) for (int x=0;x<width;x++)
        for (int c=0;c<4;c++) linear_source[(size_t(y)*width+x)*4+c] =
            muglm::floatToHalf(c==3 ? 1.0f : rgb_at(x,y,c));
    auto image_info = ImageCreateInfo::immutable_2d_image(width, height,
        source_space == 2 ? VK_FORMAT_R16G16B16A16_SFLOAT :
        ten ? VK_FORMAT_A2B10G10R10_UNORM_PACK32 : VK_FORMAT_R8G8B8A8_UNORM);
    image_info.initial_layout = VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL;
    ImageInitialData initial_data = {source_space == 2 ? static_cast<const void*>(linear_source.data()) : static_cast<const void*>(source.data())};
    auto input_image = device.create_image(image_info, &initial_data);
    ASSERT_THAT(input_image);
    ImageHandle outputs[3];
    for (int c = 0; c < 3; c++) {
        auto output_info = ImageCreateInfo::immutable_2d_image(c && !c444 ? width/2 : width,
            c && !c444 ? height/2 : height, VK_FORMAT_R16_UNORM);
        output_info.usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_STORAGE_BIT;
        output_info.initial_layout = VK_IMAGE_LAYOUT_GENERAL;
        outputs[c] = device.create_image(output_info);
        ASSERT_THAT(outputs[c]);
    }
	pyrowave_rate_control rate_control = {frameBudget ? frameBudget : size_t(width) * height * 16 + 65536};
	pyrowave_gpu_buffers gpu_buffers = {};

	pyrowave_image_view view = {};

	view.image = input_image->get_image();
	view.width = input_image->get_width();
	view.height = input_image->get_height();
	view.image_format = input_image->get_format();
	view.view_format = input_image->get_format();
	view.layer = 0;
	view.layout = input_image->get_layout(VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL);
	view.mip_level = 0;
	view.aspect = VK_IMAGE_ASPECT_COLOR_BIT;

	auto cmd = device.request_command_buffer();

	// Encode to provided cmd.
	// Redirect commands here.
	pyrowave_scaled_encode_info scaling = {};
	scaling.intermediate_plane_format = ten ? VK_FORMAT_R16_UNORM : VK_FORMAT_R8_UNORM;
	scaling.view = view;
	scaling.input_color_space = source_space == 2 ? VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT :
        source_space == 1 ? VK_COLOR_SPACE_HDR10_ST2084_EXT : VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
    scaling.output_color_space = hdr ? VK_COLOR_SPACE_HDR10_ST2084_EXT : VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
	scaling.ycbcr_chroma_midpoint = float(1u << (ten ? 9 : 7)) / max;
	scaling.force_linear_filtering = false;
    scaling.skip_dither = true;
	

	symbol<decltype(&pyrowave_device_set_command_buffer)>(encodeLibrary, "pyrowave_device_set_command_buffer")(pyro_device, cmd->get_command_buffer());
	CHECKED(pyrowave_encoder_encode_gpu_scaled(encoder, nullptr, nullptr, &scaling, &rate_control));
	symbol<decltype(&pyrowave_device_set_command_buffer)>(encodeLibrary, "pyrowave_device_set_command_buffer")(pyro_device, VK_NULL_HANDLE);

	// Wait on CPU before we call packetization.
	Fence fence;
	device.submit(cmd, &fence);
	fence->wait();

	size_t num_packets;
	CHECKED(pyrowave_encoder_compute_num_packets(encoder, rate_control.maximum_bitstream_size, &num_packets));
	ASSERT_THAT(num_packets == 1);

	std::unique_ptr<uint8_t[]> bitstream(new uint8_t[rate_control.maximum_bitstream_size]);
	pyrowave_packet packet;
	CHECKED(pyrowave_encoder_packetize(encoder, &packet, rate_control.maximum_bitstream_size, &num_packets,
		bitstream.get(), rate_control.maximum_bitstream_size));

    const std::string label = std::string(hdr ? "hdr" : "sdr") + (c444 ? "444" : "420");
    const std::string fixture = fixtureRoot + "/" + label + ".bin";
    if (reuseFixture) {
        std::ifstream input(fixture, std::ios::binary | std::ios::ate);
        ASSERT_THAT(input.good()); packet.offset = 0; packet.size = size_t(input.tellg());
        ASSERT_THAT(packet.size <= rate_control.maximum_bitstream_size);
        input.seekg(0); input.read(reinterpret_cast<char*>(bitstream.get()), packet.size);
        ASSERT_THAT(input.good());
    } else {
        std::ofstream output(fixture, std::ios::binary);
        output.write(reinterpret_cast<const char*>(bitstream.get()) + packet.offset, packet.size);
        ASSERT_THAT(output.good());
    }
	CHECKED(pyrowave_decoder_push_packet(decoder, bitstream.get() + packet.offset, packet.size));
	ASSERT_THAT(pyrowave_decoder_decode_is_ready(decoder, false));


    for (int c = 0; c < 3; c++) {
        auto &plane = gpu_buffers.planes[c];
        plane.image = outputs[c]->get_image();
        plane.width = outputs[c]->get_width(); plane.height = outputs[c]->get_height();
        plane.image_format = plane.view_format = VK_FORMAT_R16_UNORM;
        plane.aspect = VK_IMAGE_ASPECT_COLOR_BIT; plane.layout = VK_IMAGE_LAYOUT_GENERAL;
    }
    const auto *header = reinterpret_cast<const uint32_t *>(bitstream.get());
    ASSERT_THAT(((header[1] >> 26) & 1) == unsigned(c444));
    // Revision 186f0393 leaves color metadata zero; its session SDP owns HDR.
    ASSERT_THAT(((header[1] >> 27) & 7) == 0 || ((header[1] >> 27) & 7) == (hdr ? 7u : 0u));
	cmd = device.request_command_buffer();
	// Redirect commands here.
	symbol<decltype(&pyrowave_device_set_command_buffer)>(decodeLibrary, "pyrowave_device_set_command_buffer")(decodeDevice, cmd->get_command_buffer());
	CHECKED(pyrowave_decoder_decode_gpu_buffer(decoder, nullptr, nullptr, &gpu_buffers));
	symbol<decltype(&pyrowave_device_set_command_buffer)>(decodeLibrary, "pyrowave_device_set_command_buffer")(decodeDevice, VK_NULL_HANDLE);


    BufferHandle readbacks[3];
    for (int c = 0; c < 3; c++) {
        auto &output = *outputs[c];
        cmd->image_barrier(output, VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
            VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_READ_BIT);
        BufferCreateInfo b {}; b.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
        b.size = size_t(output.get_width()) * output.get_height() * sizeof(uint16_t);
        b.domain = BufferDomain::CachedHost;
        readbacks[c] = device.create_buffer(b);
        cmd->copy_image_to_buffer(*readbacks[c], output, 0, {}, {output.get_width(), output.get_height(),1},
            0,0,{VK_IMAGE_ASPECT_COLOR_BIT,0,0,1});
    }
    cmd->barrier(VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT,
        VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_2_HOST_READ_BIT);
    fence.reset(); device.submit(cmd, &fence); fence->wait();
    std::ofstream output(fixtureRoot + "/" + label + ".decoded", std::ios::binary);
    for (int c = 0; c < 3; ++c) {
        const auto* samples = device.map_host_buffer(*readbacks[c], MEMORY_ACCESS_READ_BIT);
        output.write(static_cast<const char*>(samples), size_t(outputs[c]->get_width()) * outputs[c]->get_height() * sizeof(uint16_t));
        device.unmap_host_buffer(*readbacks[c], MEMORY_ACCESS_READ_BIT);
    }
    ASSERT_THAT(output.good());
    pyrowave_decoder_destroy(decoder);
    pyrowave_encoder_destroy(encoder);
    symbol<decltype(&pyrowave_device_destroy)>(decodeLibrary, "pyrowave_device_destroy")(decodeDevice);
    pyrowave_device_destroy(pyro_device);
    printf("cross-codec %s: %zu encoded bytes, GPU decode completed\n", label.c_str(), packet.size);
}
int main(int argc, char** argv) {
    if (argc != 5 && argc != 9) { fprintf(stderr, "Usage: cross-codec ENCODER.so DECODER.so FIXTURE_DIRECTORY encode|decode [width height bitrate-kbps fps]\n"); return 2; }
    encodeLibrary = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL | RTLD_DEEPBIND);
    decodeLibrary = dlopen(argv[2], RTLD_NOW | RTLD_LOCAL | RTLD_DEEPBIND);
    if (!encodeLibrary || !decodeLibrary) { fprintf(stderr, "%s\n", dlerror()); return 1; }
    fixtureRoot = argv[3]; reuseFixture = std::string(argv[4]) == "decode";
    int width = 256, height = 144;
    if (argc == 9) {
        width = std::atoi(argv[5]); height = std::atoi(argv[6]);
        const auto kbps = std::strtoull(argv[7], nullptr, 10);
        const int fps = std::atoi(argv[8]);
        ASSERT_THAT(width >= 128 && width <= 3840 && height >= 128 && height <= 2160);
        ASSERT_THAT(kbps >= 1000 && kbps <= 2000000 && fps > 0 && fps <= 240);
        frameBudget = size_t(kbps * 1000 / (uint64_t(fps) * 8)) & ~size_t(3);
        ASSERT_THAT(frameBudget >= 1024 && frameBudget <= 3 * 1024 * 1024 - 8);
    }
    ASSERT_THAT(std::string(argv[4]) == "encode" || reuseFixture);
    for (bool hdr : {false, true}) for (bool c444 : {false, true})
        test_color_pipeline(hdr, hdr, c444, width, height);
}
