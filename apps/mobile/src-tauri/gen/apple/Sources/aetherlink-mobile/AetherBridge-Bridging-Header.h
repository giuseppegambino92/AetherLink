// C ABI exported by the Rust app library (apps/mobile/src-tauri/src/ios.rs).
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

void aether_ios_host_display(uint32_t width, uint32_t height, float scale, float refresh_hz);
void aether_ios_host_config(bool hevc, uint32_t width, uint32_t height, uint16_t fps);
void aether_ios_host_codec_config(const uint8_t *ptr, size_t len);
void aether_ios_host_frame(const uint8_t *ptr, size_t len, uint64_t pts_us, bool keyframe);
void aether_ios_host_audio(const float *ptr, size_t count, uint32_t sample_rate, uint8_t channels, uint64_t pts_us);
void aether_ios_host_stopped(void);
bool aether_ios_audio_wanted(void);
void aether_ios_set_default_device_name(const char *name);
void aether_ios_qr_result(const char *content, const char *error);
