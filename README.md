# AstroProbe
在外部通过 Websocket 获取 AstroDX 的实时判定数据。

目前已支持：
- 实时判定统计（Critical Perfect、Perfect、Great Good、Miss 等）

## 原理
AstroProbe 通过修改 AstroDX APK 的 `libmain.so` 的 `DT_NEEDED` 加载自己的 so，通过 [And64InlineHook](https://github.com/Rprop/And64InlineHook) Hook 部分 il2cpp 方法实现注入获取游戏内信息。

## 安装
[Github Releases](https://github.com/purofle/AstroProbe/releases)

请先备份原本的 AstroDX 数据，再卸载安装此修改版。

## 编译
TODO...
计划通过一个脚本实现

## 许可证
本项目的 MIT 许可证仅适用于 AstroProbe 自身的代码，AstroDX 的版权归 [2394425147](https://github.com/2394425147) 所有。
