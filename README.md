# GlazeTray

GlazeWM 的轻量系统托盘状态与控制工具（Windows 11，Rust）。

静止时只有一个动态托盘图标；左键点击打开弹层查看/切换各显示器工作区与平铺方向。
工作区、平铺方向或暂停状态变化时，状态弹层会短暂显示并自动隐藏。
不占用桌面边距，不依赖 Zebar / Node.js / WebView。

## 功能

- 动态托盘图标：当前焦点工作区 + 水平/垂直方向标记；暂停时显示琥珀色暂停标记。
- 左键弹层：按显示器列出工作区、当前激活工作区和平铺方向，并显示运行/暂停状态。
- 临时提示：工作区、方向和暂停状态变化时自动置顶显示，点击穿透且不抢键盘焦点。
- 右键菜单：打开 GlazeTray / 重新连接 / 打开配置目录 / 开机启动 / 关于 / 退出。
- 中键：切换当前焦点方向。
- Tooltip 显示完整状态文本。
- GlazeWM 断线自动指数退避重连；Explorer 重启后托盘图标自动恢复。
- 单实例运行（第二个实例激活第一个实例的弹层）。
- 配置热加载、亮/暗主题、系统强调色、高对比度与“减少动画”适配。

## 构建

```sh
cargo build --release
```

产物：`target/release/glazetray.exe`（GUI 子系统，无控制台窗口）。

## 配置

默认配置位置：`%USERPROFILE%\.glzr\glazetray\config.yaml`（缺失时使用内置默认值，
首次打开“配置目录”会自动生成）。

```yaml
glazewm:
  url: "ws://127.0.0.1:6123"   # 仅允许 loopback 地址
  reconnect-initial-ms: 250
  reconnect-max-ms: 10000

tray:
  show-direction: true
  use-system-accent: true
  scroll-switch-workspace: false   # 任务栏滚轮切换工作区（默认关闭）

flyout:
  width: 460
  show-empty-workspaces: true
  close-on-workspace-switch: false
  animation: "system"              # system | on | off

startup:
  launch-with-windows: true

logging:
  level: "info"
  retention-days: 7
```

日志位于 `%LOCALAPPDATA%\glazetray\logs`（按天轮转）。

## 测试

```sh
cargo test
```

包含 reducer 单元测试、布局/定位/编码测试以及基于模拟 WebSocket 服务的 IPC 集成测试
（初始同步、命令往返、事件更新、服务重启后重连与状态替换）。

## 协议

GlazeWM 3.10.x：`sub -e <events>` 订阅；查询响应为
`{ "messageType": "client_response", "data": { "monitors": ... }, ... }` 对象包装格式；
事件为 `{ "messageType": "event_subscription", "data": { "eventType": ... } }`。

## 许可证

MIT。文本渲染使用 DirectWrite，按需加载系统字体并自动完成中英文字形回退。
