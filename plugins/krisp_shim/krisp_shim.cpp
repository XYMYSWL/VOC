// Krisp 降噪 → VOC插件接口 的适配层（模板）
//
// 为什么需要它：Krisp SDK 是闭源的商业 C++ 库，接口随版本变化。这里把它包成
// voc_denoise.h 定义的 5 个 C 函数，客户端就能在「设置 → AI 降噪 → Krisp / 外部插件」里加载。
//
// 使用步骤：
//   1. 在 Krisp 官网申请开发者账号并获得 SDK（含头文件、库文件、模型文件 .kef）。
//   2. 按你拿到的 SDK 版本，把下面标了 TODO(krisp) 的 4 处换成真实调用：
//      函数名、参数以 SDK 自带的头文件和示例为准（不同版本差别较大，这里不臆造具体 API）。
//   3. 用 CMakeLists.txt 编译出 voc_krisp.dll，把它和 Krisp 的 DLL 放在同一目录。
//   4. 客户端设置里填 DLL 路径和模型文件路径，点「加载插件」。
//
// 约定：输入输出都是 48 kHz、单声道、480 采样（10 ms）的 float，范围 [-1, 1]。
// Krisp 的降噪会话一般可以直接按 48 kHz / 10 ms float 创建，无需重采样。

#include "../voc_denoise.h"

#include <cstdio>
#include <cstring>
#include <string>

// TODO(krisp): #include Krisp SDK 的头文件

namespace {

struct Session {
    // TODO(krisp): 保存 Krisp 降噪会话对象 / 句柄
    uint32_t frame_size = 0;
};

int g_instances = 0;  // SDK 全局初始化引用计数（客户端切换插件时会先销毁旧实例再创建新的）

void set_err(char* err, uint32_t len, const std::string& msg) {
    if (err && len) {
        std::snprintf(err, len, "%s", msg.c_str());
    }
}

}  // namespace

extern "C" {

VOC_EXPORT uint32_t voc_denoise_abi_version(void) { return VOC_DENOISE_ABI_VERSION; }

VOC_EXPORT const char* voc_denoise_name(void) { return "Krisp"; }

VOC_EXPORT void* voc_denoise_create(const char* model_path, uint32_t sample_rate,
                                          uint32_t frame_size, char* err, uint32_t err_len) {
    if (!model_path || !*model_path) {
        set_err(err, err_len, "请在设置里填写 Krisp 模型文件路径");
        return nullptr;
    }
    if (g_instances == 0) {
        // TODO(krisp): SDK 全局初始化；失败时 set_err(...) 并 return nullptr
    }

    auto* s = new Session();
    s->frame_size = frame_size;

    // TODO(krisp): 用 model_path（UTF-8，Windows 上可能要转成 wchar_t）加载模型，
    //              按 sample_rate(48000) / 10 ms / float 创建降噪会话，保存到 s 里。
    //              失败时：set_err(err, err_len, "具体原因"); delete s; 必要时反初始化; return nullptr;
    (void)sample_rate;

    ++g_instances;
    return s;
}

VOC_EXPORT int32_t voc_denoise_process(void* handle, const float* in, float* out, uint32_t n) {
    auto* s = static_cast<Session*>(handle);
    if (!s || n != s->frame_size) return -1;

    // TODO(krisp): 调用会话处理一帧：in(n 个 float) -> out(n 个 float)，失败返回非 0。
    //              下面的拷贝只是占位（未接 SDK 前相当于不降噪）。
    std::memcpy(out, in, n * sizeof(float));
    return 0;
}

VOC_EXPORT void voc_denoise_destroy(void* handle) {
    auto* s = static_cast<Session*>(handle);
    if (!s) return;
    // TODO(krisp): 释放降噪会话
    delete s;
    if (--g_instances == 0) {
        // TODO(krisp): SDK 全局反初始化
    }
}

}  // extern "C"
