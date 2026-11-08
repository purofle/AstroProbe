# AstroProbe
在外部通过 Websocket 获取 AstroDX 的实时判定数据。

目前已支持：
- 实时判定统计（Critical Perfect、Perfect、Great Good、Miss 等）
- 歌曲开始 / 结束事件（曲名、难度、本局判定统计）

## WebSocket 消息
默认监听 `ws://0.0.0.0:8765`（路径 `/control` 和带 `?tid=` 的连接留给 DG-LAB 联动），每条消息是一个带 `type` 字段的 JSON：

- `hello`：连接时发送，`song` 为当前正在进行的歌曲（即其 `song_start`），不在游戏中时为 `null`
- `song_start`：进入游戏、谱面加载完成时发送
  ```json
  {"type":"song_start","play":3,"t_ms":349300.4,"title":"AstroDX Tutorial","artist":"","difficulty":"Basic","difficulty_id":2,"level":"2","charter":"雪魚豆腐"}
  ```
  `play` 为本局编号，每次开始加 1；`difficulty_id` 对应 simai 谱面的 `&inote_<id>`
- `judge`：每个判定一条，`play` 指明所属的局
- `song_end`：`reason` 为 `finish`（打完）、`restart`（重试）、`quit`（退出），或 `unknown`（未观察到结束就开始了下一局）
  ```json
  {"type":"song_end","play":2,"t_ms":348843.5,"reason":"restart","duration_ms":143841.6,"judged":45,"grades":{"CriticalPerfect":0,"Perfect":0,"Great":0,"Good":0,"Miss":45}}
  ```

## DG-LAB 联动
出现 Miss / Good / Great / 绝赞非 Critical Perfect 时，向郊狼输出波形。lib 自己实现了 [DG-LAB 4 APP 的 Socket 控制（V4 协议）](https://github.com/dungeonlab-open/dglab-kit) 的中继和控制方，判定直接在游戏里处理，不需要外网，也不需要一直开着网页。

1. 浏览器打开 `http://手机IP:8765/`（和游戏同一台手机上打开 `http://127.0.0.1:8765/` 也行）
2. 在 DG-LAB APP 里选 Socket 控制，扫描页面上的二维码，APP 会连到 `ws://手机IP:8765/?tid=...`；在 APP 里连接郊狼并调好通道强度（强度上限由 APP 控制，页面只按百分比缩放波形强度）
3. 设置各判定的波形强度、持续时间和波形，以及输出通道，打开「输出」开关；之后关掉网页也会继续输出

配对 ID 和设置保存在 `/sdcard/Android/data/com.Reflektone.AstroDX/files/dglab.json`，二维码不会变，游戏重启后 APP 重新连上即可。可以打开「游戏启动时自动开启输出」，否则每次启动游戏都需要在页面上打开输出。不需要这个功能时可在配置中设 `"web": false`（同时关闭网页和中继）。

内置波形数据取自 [dglab-kit](https://github.com/dungeonlab-open/dglab-kit)（GPL-3.0），二维码生成使用 [qrcode-generator](https://github.com/kazuhikoarase/qrcode-generator)（MIT）。

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
