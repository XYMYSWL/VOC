/*
 * VOC 降噪插件接口（ABI 版本 1）
 *
 * 插件是一个导出以下 C 函数的 DLL / .so / .dylib。客户端运行时用 LoadLibrary 加载，
 * 插件所在目录会加入依赖搜索路径（Krisp SDK 自带的 DLL 放同一目录即可）。
 *
 * 音频格式固定：单声道 float32，取值 [-1, 1]，采样率 48000，每帧 480 个采样（10 ms）。
 * 所有函数都只会在客户端的编码线程中调用，不需要考虑多线程。
 */
#ifndef VOC_DENOISE_H
#define VOC_DENOISE_H

#include <stdint.h>

#ifdef _WIN32
#define VOC_EXPORT __declspec(dllexport)
#else
#define VOC_EXPORT __attribute__((visibility("default")))
#endif

#define VOC_DENOISE_ABI_VERSION 1

#ifdef __cplusplus
extern "C" {
#endif

/* 必须返回 VOC_DENOISE_ABI_VERSION */
VOC_EXPORT uint32_t voc_denoise_abi_version(void);

/* 可选：显示在设置界面的名称（UTF-8，静态字符串） */
VOC_EXPORT const char* voc_denoise_name(void);

/*
 * 创建降噪实例。
 *   model_path   UTF-8 模型文件路径（用户在设置里填写，可能为空字符串）
 *   sample_rate  采样率（当前恒为 48000）
 *   frame_size   每帧采样数（当前恒为 480）
 *   err / err_len 失败时把 UTF-8 错误信息写到这里（以 \0 结尾）
 * 失败返回 NULL。
 */
VOC_EXPORT void* voc_denoise_create(const char* model_path, uint32_t sample_rate,
                                          uint32_t frame_size, char* err, uint32_t err_len);

/* 处理一帧：in/out 各 n 个采样。成功返回 0；非 0 时客户端会停用插件并透传原声。 */
VOC_EXPORT int32_t voc_denoise_process(void* handle, const float* in, float* out, uint32_t n);

/* 释放实例 */
VOC_EXPORT void voc_denoise_destroy(void* handle);

#ifdef __cplusplus
}
#endif

#endif
