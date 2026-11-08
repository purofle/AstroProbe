# AstroProbe
在外部通过 Websocket 获取 AstroDX 的实时判定数据。

目前已支持：
- 实时判定统计（Critical Perfect、Perfect、Great Good、Miss 等）

## 原理
AstroProbe 通过修改 AstroDX APK 的 `libmain.so` 的 `DT_NEEDED` 加载自己的 so，通过 [ShadowHook](https://github.com/bytedance/android-inline-hook) Hook 部分 il2cpp 方法实现注入获取游戏内信息。

加载时序：`libjudgehook.so` 的 `.init_array` 先改写 `libmain.so` 中 `dlopen` 的 GOT 表项，`libil2cpp.so` 一被加载就 hook `il2cpp_init`，在其返回后（il2cpp 主线程上，运行时已初始化完成）按类名/方法名解析目标方法并安装 hook，不依赖固定的 RVA，也无需轮询。

## 安装
[Github Releases](https://github.com/purofle/AstroProbe/releases)

请先备份原本的 AstroDX 数据，再卸载安装此修改版。

## 编译
TODO...
计划通过一个脚本实现

## 许可证
本项目的 MIT 许可证仅适用于 AstroProbe 自身的代码，AstroDX 的版权归 [2394425147](https://github.com/2394425147) 所有。
