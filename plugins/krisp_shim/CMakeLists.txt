cmake_minimum_required(VERSION 3.16)
project(voc_krisp CXX)
set(CMAKE_CXX_STANDARD 17)

# 指向你解压的 Krisp SDK，例如 -DKRISP_SDK_DIR=D:/krisp-sdk
set(KRISP_SDK_DIR "" CACHE PATH "Krisp SDK 根目录")

add_library(voc_krisp SHARED krisp_shim.cpp)
if(KRISP_SDK_DIR)
  # TODO(krisp): 按 SDK 实际的目录结构调整 include / lib 路径和库名
  target_include_directories(voc_krisp PRIVATE "${KRISP_SDK_DIR}/include")
  target_link_directories(voc_krisp PRIVATE "${KRISP_SDK_DIR}/lib")
  # target_link_libraries(voc_krisp PRIVATE <krisp 库名>)
endif()
