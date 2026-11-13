# Windows 风格剪贴板历史设计

本设计为 MacToWindows 增加纯文本剪贴板历史。服务独立于 Windows 键位映射运行，使用左 Option+V 作为 Win+V，全局打开独立浮动面板。

- 只保存非空纯文本；最多 50 条，每条最多 1,048,576 个 Unicode 标量值。
- 重复文本移到最前并更新时间；选择历史后写回系统剪贴板、恢复原应用并发送 Command+V。
- 历史写入 `~/Library/Application Support/MacToWindows/clipboard-history.json`，使用临时文件、同步和原子替换。
- NSPasteboard 读取/写入和窗口操作全部在后台或主线程执行，Event Tap 回调只做无阻塞热键识别和通知发送。
- 不保存图片、文件、富文本，不记录完整按键序列，不做密码来源识别。
