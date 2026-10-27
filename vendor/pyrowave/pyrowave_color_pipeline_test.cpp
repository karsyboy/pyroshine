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
	CHECKED(pyrowave_create_device(&info, &pyro_device));

	pyrowave_encoder_create_info encoder_info = {};
	encoder_info.device = pyro_device;
	encoder_info.chroma = c444 ? PYROWAVE_CHROMA_SUBSAMPLING_444 : PYROWAVE_CHROMA_SUBSAMPLING_420;
	encoder_info.width = width;
	encoder_info.height = height;
	CHECKED(pyrowave_encoder_create(&encoder_info, &encoder));

	pyrowave_decoder_create_info decoder_info = {};
	decoder_info.device = pyro_device;
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
	pyrowave_rate_control rate_control = {size_t(width) * height * 16 + 65536};
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
	

	pyrowave_device_set_command_buffer(pyro_device, cmd->get_command_buffer());
	CHECKED(pyrowave_encoder_encode_gpu_scaled(encoder, nullptr, nullptr, &scaling, &rate_control));
	pyrowave_device_set_command_buffer(pyro_device, VK_NULL_HANDLE);

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
    ASSERT_THAT(((header[1] >> 27) & 7) == (hdr ? 7u : 0u));
	cmd = device.request_command_buffer();
	// Redirect commands here.
	pyrowave_device_set_command_buffer(pyro_device, cmd->get_command_buffer());
	CHECKED(pyrowave_decoder_decode_gpu_buffer(decoder, nullptr, nullptr, &gpu_buffers));
	pyrowave_device_set_command_buffer(pyro_device, VK_NULL_HANDLE);


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
    const auto pq_encode = [](double nits) {
        double x=std::pow(std::max(0.0,nits)/10000.0,2610.0/16384);
        return std::pow((3424.0/4096 + 2413.0/128*x)/(1+2392.0/128*x),2523.0/32);
    };
    const auto pq_decode = [](double pq) {
        double x=std::pow(pq,32.0/2523);
        return 10000*std::pow(std::max(0.0,x-3424.0/4096)/(2413.0/128-2392.0/128*x),16384.0/2610);
    };
    const auto srgb_decode=[](double x) { return x<=0.04045 ? x/12.92 : std::pow((x+0.055)/1.055,2.4); };
    const auto srgb_encode=[](double x) { return x<=0.0031308 ? 12.92*x : 1.055*std::pow(x,1/2.4)-0.055; };
    const float kr = hdr ? 0.2627f : 0.2126f, kb = hdr ? 0.0593f : 0.0722f;
    float worst[3] {};
    for (int c = 0; c < 3; c++) {
        auto decoded = static_cast<const uint16_t *>(device.map_host_buffer(*readbacks[c], MEMORY_ACCESS_READ_BIT));
        const int factor = c && !c444 ? 2 : 1;
        const int w = width / factor, h = height / factor;
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
            float expected = 0;
            for (int dy = 0; dy < factor; dy++) for (int dx = 0; dx < factor; dx++) {
                float rgb[3]; auto packed = source[size_t(y*factor+dy)*width+x*factor+dx];
                for (int k=0;k<3;k++) rgb[k] = float((packed >> (k*(ten?10:8))) & max) / max;
                if (source_space != (hdr ? 1 : 0)) {
                    double lin[3];
                    for (int k=0;k<3;k++) lin[k] = source_space==0 ? 200*srgb_decode(rgb[k]) :
                        source_space==1 ? pq_decode(rgb[k]) : 80*rgb_at(x*factor+dx,y*factor+dy,k);
                    double dest[3];
                    if (source_space==1) {
                        dest[0]=1.660491*lin[0]-0.587641*lin[1]-0.072850*lin[2];
                        dest[1]=-0.124550*lin[0]+1.132900*lin[1]-0.008349*lin[2];
                        dest[2]=-0.018151*lin[0]-0.100579*lin[1]+1.118730*lin[2];
                    } else if (hdr) {
                        dest[0]=0.627404*lin[0]+0.329283*lin[1]+0.043313*lin[2];
                        dest[1]=0.069097*lin[0]+0.919540*lin[1]+0.011362*lin[2];
                        dest[2]=0.016391*lin[0]+0.088013*lin[1]+0.895595*lin[2];
                    } else for (int k=0;k<3;k++) dest[k]=lin[k];
                    for (int k=0;k<3;k++) rgb[k] = std::max(0.0,std::min(1.0,
                        hdr ? pq_encode(dest[k]) : srgb_encode(std::max(0.0,dest[k])/200)));
                }
                float yy = kr*rgb[0] + (1-kr-kb)*rgb[1] + kb*rgb[2];
                float v = c == 0 ? yy : scaling.ycbcr_chroma_midpoint +
                    (c == 1 ? (rgb[2]-yy)/(2*(1-kb)) : (rgb[0]-yy)/(2*(1-kr)));
                // R8 scaler quantizes each nonlinear YUV texel; 4:2:0 averages
                // before storage. R16 adds only its own UNORM rounding.
                expected += v / (factor*factor);
            }
            float intermediate_max = ten ? 65535.0f : 255.0f;
            expected = std::round(std::max(0.0f,std::min(1.0f,expected))*intermediate_max)/intermediate_max;
            float actual = decoded[size_t(y)*w+x] / 65535.0f;
            worst[c] = std::max(worst[c], std::abs(expected-actual));
        }
        // Lossy CDF 9/7 codec with FP16 wavelet storage; test all pixels and
        // retain measured errors instead of treating call success as proof.
        fprintf(stderr, "%dx%d hdr%d ten%d 444%d src%d plane%d maxerr %.8f\n",width,height,hdr,ten,c444,source_space,c,worst[c]);
        ASSERT_THAT(worst[c] < (ten ? 8.0f/1023 : 3.0f/255));
    }
    printf("%dx%d %s %s %s max Y/Cb/Cr error %.7f %.7f %.7f\n",width,height,
        hdr?"PQ2020":"SDR709",ten?"R16":"R8",c444?"444":"420",worst[0],worst[1],worst[2]);
	pyrowave_encoder_destroy(encoder);
	pyrowave_decoder_destroy(decoder);
	pyrowave_device_destroy(pyro_device);

}


int main() {
    for (auto extent : {std::pair<int,int>{130,134}, {1920,1080}, {2560,1440}, {3440,1440}, {3840,2160}})
        for (bool c444 : {false,true}) for (int mode=0;mode<3;mode++)
            test_color_pipeline(mode==2,mode!=0,c444,extent.first,extent.second);
    for (bool c444 : {false,true}) {
        test_color_pipeline(true,true,c444,130,134,0); // SDR -> PQ/2020
        test_color_pipeline(true,true,c444,130,134,2); // scRGB -> PQ/2020
        test_color_pipeline(false,false,c444,130,134,1); // PQ -> SDR
        test_color_pipeline(false,false,c444,130,134,2); // scRGB -> SDR
    }
    test_color_pipeline(false,false,true,131,133);
    test_color_pipeline(true,true,true,131,133);
}
