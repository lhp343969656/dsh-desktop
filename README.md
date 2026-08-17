# dsh-desktop2

DeepSeek Harness（dsh）桌面客户端。Tauri 2 壳 + 内置 Node Runtime，安装后双击即用，
无需用户安装 Node、dsh 或浏览器（Windows 用系统 WebView2，macOS 用系统 WKWebView）。

## 为什么是这个方案

- 安装包 ~40–60MB，安装几秒（对比 Electron 方案 200MB+、装几分钟）
- 用户机器零依赖：不需要任何浏览器，WebView 是操作系统组件
- 关闭窗口即退出全部 dsh 进程（Windows 用 Job Object，macOS 用进程组）

## 目录结构

```
├── ui/                    # 壳自带页面（启动加载页 / 错误页），dsh 页面是外部 URL
├── scripts/
│   ├── build-runtime.mjs  # 打包内置 Runtime：node + dsh 依赖闭包 + manifest
│   └── smoke-runtime.mjs  # 冒烟测试：真实启动 dsh web，验证就绪协议与健康检查
├── resources/
│   └── bundled-runtime/   # 构建产物（gitignore），安装时随包分发
└── src-tauri/             # Rust 壳
    └── src/
        ├── shell.rs       # host 进程启动 / 整树终止（平台适配）
        ├── ready.rs       # 就绪协议解析 + URL 校验 + HTTP 健康检查
        └── runtime.rs     # Runtime 选择：环境变量覆盖（开发）> 内置 Runtime
```

## 开发

```powershell
npm install

# 开发模式（走本地 dsh 源码，不经打包）：
$env:DSH_DESKTOP_DSH_BIN = 'D:\deepseek-harness\apps\cli\lib\bin.js'
$env:DSH_DESKTOP_NODE_BIN = 'D:\Program Files\nodejs\node.exe'
$env:DSH_DESKTOP_DATA_ROOT = "$PWD\.dev-data"
npm run dev
```

## 构建

```powershell
# 1. 打包内置 Runtime（需要 dsh 工作区，见 scripts/build-runtime.mjs 环境变量）
npm run runtime:build

# 2. 冒烟测试 Runtime（可选）
npm run runtime:smoke

# 3. 构建安装包（Windows 出 NSIS .exe，macOS 出 .dmg）
npm run build
```

产物在 `src-tauri/target/release/bundle/`。

## 启动流程（壳）

1. 单实例锁（第二次启动聚焦已有窗口）
2. 选 Runtime：环境变量覆盖 > 安装包内置 Runtime（校验平台/架构/manifest）
3. 以参数数组启动 `node dsh --profile web --host 127.0.0.1 --port 0`（不经 shell）
4. 解析就绪行（JSONL 或文本格式），URL 仅接受 `http://127.0.0.1:<port>/`
5. HTTP 健康检查 2xx 后，窗口导航到该 URL
6. 窗口关闭 → 应用退出 → 整树终止 host（错误信息显示在加载页，日志在用户目录 runtime.log）

用户数据独立：会话/凭证在 `%APPDATA%/org.dshdesktop.app/dsh-home`（macOS 为
Application Support 对应目录），与程序、Runtime 分离，更新不覆盖。

## 发布（生产化需要准备）

- [ ] **GitHub 仓库**：CI 构建与自动更新分发需要（公开或私有皆可）
- [ ] **Windows 代码签名**：Azure Trusted Signing（便宜，按次计费）或 EV 证书；
      未签名安装包会触发 SmartScreen 警告
- [ ] **Apple Developer Program**（$99/年）：macOS 分发必须，签名 + notarization 公证，
      否则 Gatekeeper 拦截
- [ ] **Tauri updater**：壳自动更新（静态发布目录 / GitHub Releases + latest.json）

## 里程碑

- [x] M1：壳 + 启动 Runtime + 单窗口 + 关窗全退（Windows 本地）
- [x] M2：NSIS 安装包（未签名）
- [ ] M3：Windows 代码签名 + 自动更新
- [ ] M4：macOS DMG + 公证
- [ ] M5：CI 发布流水线 + Runtime 独立更新通道
