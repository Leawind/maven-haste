---
grounds:
  - CROSS_PLATFORM
rationale: 同平台内文件系统行为也有差异，运行时探测比 OS 假设更准确。
summary: 跨平台策略：运行时探测代替 OS 假设，主动拒绝平台不安全路径，依赖自包含
---

平台差异运行时探测而非编译期假设（大小写敏感性、原子 rename，src/storage.rs）；路径校验主动拒绝 Windows 不安全段（src/request_path.rs）；依赖自包含（bundled SQLite、rustls）；CI 覆盖 Linux/macOS/Windows。
