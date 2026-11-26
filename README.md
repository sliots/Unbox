<div align="center">

![Unbox Logo](appicon/package-box.png)

# Unbox

<h3><i>Double-Click to Extract, Fluent & Native</i></h3>

**面向 Windows 11 的复制对话框风格 · 双击即解压代理（Rust 重写版）**

<p>
  <a href="https://github.com/sliots/Unbox/releases">
    <img src="https://img.shields.io/badge/Version-v2.0.0-00C853?style=for-the-badge&logo=github&labelColor=000000" alt="Release">
  </a>
  <img src="https://img.shields.io/badge/Platform-Windows_11_%7C_10-0078D4?style=for-the-badge&logo=windows&labelColor=000000" alt="Platform">
  <img src="https://img.shields.io/badge/Core-Rust_%7C_Slint-B7410E?style=for-the-badge&logo=rust&labelColor=000000" alt="Core">
  <a href="https://github.com/mcmilk/7-Zip-zstd">
    <img src="https://img.shields.io/badge/Engine-7--Zip_ZS_26.05-FF8C00?style=for-the-badge&labelColor=000000" alt="Engine">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/License-MIT-3DA639?style=for-the-badge&logo=opensourceinitiative&labelColor=000000" alt="License">
  </a>
</p>

<img src="assets/banner.png" width="900" alt="Unbox Preview">

</div>

---

## 💡 为什么需要 Unbox

在 Windows 上解压，总要在右键二级菜单里找“解压到当前文件夹”，一不小心上千个文件就铺满了下载目录；传统解压软件常驻后台、弹广告、弹向导。

**Unbox 的回答：双击即可，解完即走。** 它像 macOS 的 Archive Utility 一样自然，外观则贴近 Windows 11 自带的文件复制对话框。

## ✨ 2.0：Rust 完全重写

| | 1.x | 2.0 |
|:--|:--|:--|
| 外壳 | WPF / .NET 10（需安装桌面运行时） | **Rust + Slint**（Skia 渲染），`unbox.exe` 自带 VC++ 运行库，无需额外安装 |
| 解压核心 | 独立 C++ `UnzipWorker.exe` | 同一个 `unbox.exe --worker` 子进程，仍隔离加载 7z.dll，并由任务对象托管 |
| 文件关联 | PowerShell 脚本 + `安装与卸载.cmd` | `unbox register / unregister / status`，图标内嵌在 exe 中 |
| 分发 | 手动下载便携包 | GitHub Actions 自动构建、测试、发布；**支持 Scoop 安装** |
| 命令行 | — | `unbox extract <压缩包>` 无窗口解压（同一条校验与提交路径） |

### 界面

- **与 1.x 一致的外观**：按 1.x WPF 窗口 1:1 还原——自绘 30 px 标题栏（时钟图标、标题、最小化 / 关闭）、1 px 边框、12 px 进度条、92 px 吞吐面积图与三行详细信息，浅色 / 深色配色与 1.x 相同。
- **Skia 渲染**：文字与图形抗锯齿清晰，使用 CPU 光栅化，不依赖显卡驱动，虚拟机与远程桌面同样可用。
- **详细信息展开/折叠**：窗口高度与内容 220 ms 缓动过渡，与 1.x 相同。
- **询问在窗口内完成**：密码、文件名编码、非法文件名、回收站提示、解压上限沿用 1.x 对话框的内容与布局，直接显示在主窗口内，窗口高度随内容调整。
- **任务栏按钮同步显示进度**（暂停变黄、失败变红），出错时闪烁提醒；Windows 11 下窗口为圆角。

## ⚡ 产品行为（与 1.x 一致）

<table>
<tr><td width="28%"><b>📁 目录结构与防冲突</b></td><td>
以包名建立根目录并完整保留包内层级；仅当包内<b>恰好只有一个与包名同名的子目录</b>时折叠这一级；<code>.tar.gz</code> / <code>.tgz</code> 等按内容确认内层 TAR 后展开；目标已存在时自动递增为 <code>名称 (2)</code>，<b>绝不覆盖</b>。
</td></tr>
<tr><td><b>🗑️ 移入回收站与安全校验</b></td><td>
只回收本次真正读取过的源卷；回收前用<b>卷序列号 + 128 位文件 ID + 长度 + 修改时间</b>核对身份，并对交付内容逐项做长度与 CRC32 复核，通过 Shell <code>IFileOperation</code> 回收并核实回收站落点。网络路径、回收站被禁用或容量未知、部分完成、任一校验不一致时<b>保留源包</b>。
</td></tr>
<tr><td><b>🎯 57 种格式</b></td><td>
常用归档、分卷入口、压缩流、发行包与磁盘映像；资源管理器可原生打开的 <code>.iso</code> / <code>.vhd(x)</code> 与 Office 容器不登记。
</td></tr>
<tr><td><b>🍏 编码与 macOS 文件</b></td><td>
尊重 ZIP bit 11、Unicode Path 扩展字段与 PAX <code>path</code> 的编码声明；未声明的名称按系统代码页解码，确实不确定时让用户选择（附示例名）。自动过滤 <code>__MACOSX/</code>、<code>.DS_Store</code>、<code>._*</code>。
</td></tr>
<tr><td><b>🛡️ 路径安全</b></td><td>
拒绝绝对路径、<code>..</code>、设备名与链接条目；所有目录逐级相对已验证的父句柄创建，并以句柄最终路径核对，联接点/符号链接无法把写入引出暂存区。非法字符可批量替换为 <code>-</code> 或跳过（此时显示“部分完成”并保留源包）。
</td></tr>
<tr><td><b>📏 输出预算</b></td><td>
按声明大小设置写出上限，防止声明失真写满磁盘；大小未知的流超限时可输入更大的上限后从头重新解压。
</td></tr>
</table>

## 📦 安装

### Scoop（推荐）

```powershell
scoop bucket add unbox https://github.com/sliots/Unbox   # 默认分支需包含 bucket/
scoop install unbox/unbox
# 或者直接用清单地址安装 Rust 分支：
scoop install https://raw.githubusercontent.com/sliots/Unbox/rust/bucket/unbox.json
```

安装后会自动为当前用户登记“打开方式”。随后在 **设置 → 应用 → 默认应用 → Unbox** 中把需要双击解压的扩展名设为 Unbox。`scoop update` 后关联仍指向 `current` 目录，无需重新登记；`scoop uninstall unbox` 会自动移除关联。

### 便携包

从 [Releases](https://github.com/sliots/Unbox/releases) 下载 `Unbox-v2.0.0-win-x64.zip`（可用同页的 `SHA256SUMS.txt` 校验），解压到常用目录后执行：

```powershell
.\unbox.exe register      # 登记关联并打开“默认应用”设置
.\unbox.exe status        # 查看登记状态
.\unbox.exe unregister    # 移除关联
```

旧版（1.x）的关联会被 `unregister` 一并清理。设置文件沿用 `%LOCALAPPDATA%\UnzipProxy\settings.json`，旧版偏好可直接继承。

### 命令行

```powershell
unbox extract D:\Downloads\project.7z --password secret
# 成功时在标准输出打印目标目录；退出码 0 成功 / 2 部分完成 / 1 失败
```

`--skip-invalid` 跳过非法文件名（默认批量重命名），`--cp 936` 指定文件名代码页，`--recycle` 校验通过后回收源包。

## 🗂️ 支持的格式

| 分类 | 扩展名 |
|:---|:---|
| **常用归档** | `.zip` `.zipx` `.7z` `.rar` `.tar` `.ova` `.lzh` `.lha` `.cab` `.arj` |
| **分卷入口** | `.r00` `.001` |
| **发行包与系统归档** | `.deb` `.rpm` `.cpio` `.xar` `.xip` |
| **映像与虚拟磁盘** | `.wim` `.swm` `.esd` `.dmg` `.squashfs` `.vmdk` `.vdi` `.qcow` `.qcow2` `.fat` `.ntfs` |

| 压缩流 | 普通流 | TAR 短别名 | TAR 长后缀 |
|:---|:---|:---|:---|
| gzip | `.gz` `.gzip` | `.tgz` `.tpz` | `.tar.gz` `.tar.gzip` |
| bzip2 | `.bz2` `.bzip2` | `.tbz2` `.tbz` | `.tar.bz2` `.tar.bzip2` |
| XZ | `.xz` | `.txz` | `.tar.xz` |
| UNIX Compress | `.z` | `.taz` | `.tar.z` |
| LZMA | `.lzma` `.lzma86` | — | `.tar.lzma` |
| Zstandard | `.zst` `.zstd` | `.tzst` `.tzstd` | `.tar.zst` `.tar.zstd` |
| Brotli | `.br` `.brotli` | `.tbr` | `.tar.br` `.tar.brotli` |
| LZ4 / LZ5 / Lizard | `.lz4` `.lz5` `.liz` | `.tlz4` `.tlz5` `.tliz` | `.tar.lz4` `.tar.lz5` `.tar.liz` |
| Lzip | `.lz` | `.tlz` | `.tar.lz` |

格式清单的唯一来源是 [`assets/archive-formats.json`](assets/archive-formats.json)，路由规则、文件选择框和文件关联都由它生成。

## 🛠️ 从源码构建

需要 Windows 10/11 x64、[Rust](https://rustup.rs)（stable，MSVC 工具链）与 Visual Studio Build Tools（提供 `rc.exe`）。

```powershell
cargo build --release -p unbox            # target\release\unbox.exe
copy assets\7z.dll target\release\        # 7z.dll 需与 unbox.exe 同目录
.\scripts\package.ps1                     # 生成 dist\Unbox-v<版本>-win-x64.zip 与 SHA256SUMS.txt
.\scripts\smoke-test.ps1 -PackageDir dist\Unbox-v2.0.0-win-x64
```

在 macOS / Linux 上交叉编译（需要 LLVM 提供 `clang-cl`、`llvm-rc`，首次会自动下载 MSVC CRT 与 Windows SDK）：

```bash
cargo install cargo-xwin
cargo xwin build --release -p unbox --target x86_64-pc-windows-msvc
```

项目结构：

```
crates/unbox-core   平台无关的规则（格式目录、命名、路径策略、文件名解码、清单对账、IPC 协议），可在任何系统上 cargo test
crates/unbox        Windows 程序：Slint 界面、7-Zip COM 工作进程、句柄级文件系统访问、Shell 回收、文件关联
crates/unbox/ui     app.slint 界面与动效
bucket/unbox.json   Scoop 清单（发布时由 CI 自动更新版本与哈希）
```

图标（应用图标由 `appicon/*.svg` 渲染，格式图标由 `icons/*.png` 生成）与完成提示音（`Finished.mp3` 解码为 WAV）都在构建时由 `build.rs` 生成并嵌入 exe，不再需要 resvg 或 FFmpeg。

## 🚀 CI 与发布

- **CI**（`.github/workflows/ci.yml`）：Linux 上检查格式并运行 `unbox-core` 单元测试；Windows 上 Clippy、打包，并用 `scripts/smoke-test.ps1` 端到端验证 ZIP / 中文名 / 同名折叠 / `.tar.gz` / 损坏包 / 关联登记。
- **发布**（`.github/workflows/release.yml`）：推送与 `Cargo.toml` 版本一致的标签即可：

  ```bash
  git tag v2.0.0 && git push origin v2.0.0
  ```

  工作流会构建、测试、创建 GitHub Release（zip + SHA256SUMS.txt），并把新版本号与哈希写回 `rust` 分支的 `bucket/unbox.json`。

## 🔄 跟进上游（1.x C#/C++ 版本）

Rust 版本位于 `rust` 分支，旧的 C#/C++ 源码（`src/`、`tests/`、关联脚本）已从本分支删除；`main` 继续镜像上游 [Snowail/Unbox](https://github.com/Snowail/Unbox)。合并前先看上游改了哪些逻辑，再移植到 `crates/`：

```bash
git remote add upstream https://github.com/Snowail/Unbox.git   # 只需一次
git fetch upstream
git checkout main && git merge upstream/main && git push        # main 继续镜像上游
git checkout rust
git log -p rust..main -- src tests                                # 需要移植的旧版逻辑改动
git merge main                                                    # 合并共用资源
git -c core.quotepath=off status --short | grep -E '^(DU|UD) ' | cut -c4- | xargs -r git rm -q   # 旧源码的“修改/删除”冲突一律保持删除
git commit
```

`assets/`、`icons/`、`appicon/`、`Finished.mp3` 两个版本共用，上游对格式清单、图标、7z.dll 的更新合并后即生效（更换 7z.dll 时同时更新 `scripts/package.ps1` 中的固定哈希）。

## 📜 License & Credits

- 解压引擎：[7-Zip 26.05 ZS v1.5.7 R1 x64](https://github.com/mcmilk/7-Zip-zstd/releases/tag/v26.05-v1.5.7-R1)，分发许可与对应源码见 [THIRD-PARTY.txt](assets/licenses/7zip-zs/THIRD-PARTY.txt)。
- 界面框架：[Slint](https://slint.dev)，按 Slint Royalty-free License 使用，需保留下方署名徽标。

  [![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-light.svg)](https://slint.dev)
- 图标原始设计：[smalllikeart (Flaticon)](https://www.flaticon.com/authors/smalllikeart)，由本项目按格式定制。
- 原始项目：[Snowail/Unbox](https://github.com/Snowail/Unbox)。

本项目代码采用 [MIT License](LICENSE)。
