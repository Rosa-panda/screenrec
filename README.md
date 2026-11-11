# screenrec

Windows 高质量录屏 + 视频处理工具（Rust）：命令行、图形界面、显卡插帧。

- 视频：Windows Graphics Capture 抓屏（整个显示器或单个窗口，含鼠标指针），固定帧率输出
- 音频：系统内部声音（WASAPI loopback）+ 麦克风，默认混成一条音轨，也可分成两条
- 编码：自动探测 NVENC / QSV / AMF，没有硬件编码器时退回 CPU；H.264 / HEVC / AV1，恒定质量（CQ/CRF）模式
- 色彩：BT.709 限幅 yuv420p，并把色彩元数据写进码流，播放器显示颜色与屏幕一致
- 音画同步：视频与各路音频都按同一墙钟配速，编码慢、系统无声时不会漂移

## 依赖

- Windows 10 1903+ / Windows 11
- `ffmpeg.exe`（同目录或 PATH 中；本机已安装）
- 编译：Rust GNU 工具链 + MinGW-w64 binutils（`as.exe`、`dlltool.exe`，已通过 winget 安装 WinLibs）

## 编译

```
build.cmd
```

（等价于把 WinLibs 的 `mingw64\bin` 加进 PATH 后执行 `cargo build --release`；直接 `cargo build` 会因找不到 `dlltool`/`as` 失败。）

产物：`target\release\screenrec.exe`

## 图形界面与桌面快捷方式

```
install.cmd
```

编译后把 `screenrec.exe`、`screenrec-gui.exe`、`screenrec-interp.exe`、`rife\`（插帧用）复制到
`%LOCALAPPDATA%\Programs\screenrec\`，并在桌面创建 `screenrec.lnk`，双击即可启动图形界面。
设置会自动保存；崩溃日志在 `%LOCALAPPDATA%\screenrec\gui.log`。

界面分三页：

- **录制**：选显示器/窗口、系统声音与麦克风开关及电平表、画质预设与引擎；录制中可暂停/继续；可对录制来源截图（PNG）。
- **视频处理**：选一个视频（或把文件拖进窗口、或用最近一次录制），然后：
  - 插帧：RIFE 显卡插帧，目标帧率任意填（如 30→60、30→120），或按 2×/3×/4×/8× 倍数；声音原样保留
  - 压缩转码：换编码 / 调 CQ / 缩小分辨率 / 降帧率
  - 无损剪切：不重新编码截取一段（起点对齐关键帧）
  - 转 GIF、提取音频（MP3 / M4A / WAV）
  - 以上都可以只处理「片段范围」里的一段；结果保存在源文件旁边
- **设置**：全局快捷键（开始/停止、暂停/继续、截图，默认 F9 / F10 / F8，可改可清除）、
  开始后最小化、定时停止、保存位置、ffmpeg / RIFE 组件状态。

快捷键在独立线程处理，窗口最小化、游戏全屏时也有效。

## 显卡插帧命令行

```
screenrec-interp in.mp4                  帧率翻倍
screenrec-interp in.mp4 --fps 120        30 帧变 120 帧
screenrec-interp in.mp4 --times 4        帧率 ×4
screenrec-interp in.mp4 --fps 60 --start 1:00 --end 1:30    只处理一段
screenrec-interp in.mp4 --gpu 1 --model rife-v4 --codec hevc -q 16 -o out.mp4
```

需要 `rife-ncnn-vulkan`（程序目录下的 `rife\`，含 `rife-v4.6` 模型；源码树里放在 `tools\rife\`，不入库，
从 https://github.com/nihui/rife-ncnn-vulkan/releases 下载 20221029 版解压即可）以及与 ffmpeg 同目录的 `ffprobe`。
处理时在系统临时目录按段存放 PNG 帧（几 GB），结束或取消后自动清理。双显卡笔记本上 `--gpu 0` 通常是独显。

## 录制引擎（录游戏必看）

| 引擎 | 命令行 | 特点 |
|---|---|---|
| 标准 | `--engine standard` | ffmpeg 编码，CQ 画质控制，支持 AV1 / mkv / 分轨。录桌面、教程用 |
| 游戏 | `--engine game` | 同标准，但用轻量 NVENC 预设，并让出 CPU/GPU 优先级 |
| 零拷贝 | `--engine zero-copy` | 画面全程留在显卡，Media Foundation 硬件编码。占用最低，录游戏推荐 |

零拷贝引擎的限制：仅 mp4 + H.264/HEVC；画质按码率控制（由 CQ 档位换算）；系统声音与麦克风固定混成一条音轨；
异常退出时文件无法播放（mp4 文件尾在停止时才写入）。

实测（2560x1600@60，240fps 全屏负载）：标准引擎 ffmpeg 约 3.3 核 + 本体 1 核、GPU +10%；
零拷贝引擎合计 0.2 核、GPU +4%，抓取帧数 582/595。

## 命令行使用

```
screenrec                              录主显示器，Ctrl+C 停止，输出 rec_YYYYMMDD_HHMMSS.mp4
screenrec -o demo.mp4 -d 30            录 30 秒
screenrec -m 2                         录第 2 个显示器（--list 查看编号）
screenrec -w "Chrome"                  录标题包含 Chrome 的窗口（窗口不能最小化）
screenrec --codec hevc -q 16           HEVC、更高画质（CQ 值越小越好，推荐 15-23，默认 18）
screenrec --fps 30                     30 fps
screenrec --separate-tracks            系统声音、麦克风各一条音轨
screenrec --mic-volume 1.5             麦克风放大 1.5 倍
screenrec --mic "USB"                  指定名称含 USB 的麦克风
screenrec --no-mic / --no-system-audio 不录麦克风 / 不录系统声音
screenrec --encoder cpu                强制 CPU 编码
screenrec --list                       列出显示器、窗口、音频设备
```

完整参数见 `screenrec --help`。

## 说明

- 麦克风会自动使用系统默认输入设备；如果提示 "Access is denied"，到
  设置 > 隐私和安全性 > 麦克风，打开"麦克风访问"和"允许桌面应用访问麦克风"。
- 输出 `.mkv` 也可以（`-o x.mkv`），崩溃时比 mp4 更不易损坏。
- 停止录制请用 Ctrl+C（一次即可），程序会等 ffmpeg 把文件尾写完再退出。
