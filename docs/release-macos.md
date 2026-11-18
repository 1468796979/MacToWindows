# macOS 正式发布配置

GitHub Actions 工作流 `.github/workflows/release-macos.yml` 会在 Apple Silicon runner 上完成：

1. 构建 arm64 `.app`。
2. 使用 Developer ID Application 证书签名。
3. 使用 App Store Connect API Key 调用 `notarytool` 公证。
4. staple 公证票据。
5. 生成 DMG、ZIP 和 SHA-256 校验文件。
6. 发布到 GitHub Release。

## 一次性准备

需要 Apple Developer Program 账号，并在 Apple Developer 网站创建 **Developer ID Application** 证书。证书在本机导出为 `.p12`，不要把私钥提交到仓库或发送到聊天中。

还需要在 App Store Connect → Users and Access → Integrations → App Store Connect API 创建 API Key，下载 `.p8` 文件，并记录 Key ID 和 Issuer ID。

在 GitHub 仓库的 Settings → Secrets and variables → Actions 中添加以下 secrets：

- `DEVELOPER_ID_APPLICATION_CERTIFICATE_BASE64`：Developer ID Application `.p12` 文件的 base64 内容。
- `DEVELOPER_ID_APPLICATION_CERTIFICATE_PASSWORD`：导出 `.p12` 时设置的密码。
- `APPLE_API_KEY_BASE64`：App Store Connect `.p8` 文件的 base64 内容。
- `APPLE_API_KEY_ID`：App Store Connect Key ID。
- `APPLE_API_ISSUER_ID`：App Store Connect Issuer ID。

可以在本机使用下面的命令生成 base64 内容，然后粘贴到对应的 GitHub Secret：

```bash
base64 < DeveloperIDApplication.p12 | pbcopy
base64 < AuthKey_ABC123XYZ.p8 | pbcopy
```

## 发布

打开 GitHub Actions → **Release macOS arm64** → **Run workflow**，选择包含待发布代码的分支，输入与 `src-tauri/tauri.conf.json` 和 `src-tauri/Cargo.toml` 版本一致的 tag，例如两个文件均为 `0.1.2` 时填写 `v0.1.2`。

工作流固定构建此次触发对应的提交 SHA。标签不存在时，发布步骤会在该 SHA 创建标签；标签已存在时，必须指向相同提交，否则停止发布。无需预先手动创建标签。五项签名、公证 secrets 必须全部配置，缺失时只报告变量名并停止，不会自动降级发布。

构建仅使用 `aarch64-apple-darwin`，并在签名前核对主程序恰为 `arm64`。正式产物包括 Developer ID 签名、Apple 公证并附带票据的 DMG 和 ZIP。DMG 卷根包含 `MacToWindows.app` 与指向 `/Applications` 的 `Applications` 软链；发布前会只读挂载，验证 App 结构、签名与公证票据，然后卸载。`SHA256SUMS.txt` 使用文件名，可在下载目录执行：

```bash
shasum -a 256 -c SHA256SUMS.txt
```

GitHub Actions 使用临时 keychain，构建完成后会删除，不会把证书或 API key 写入产物。

## 缺少正式签名凭据时的本地发布

没有上述 secrets 或本机没有有效 Developer ID 身份时，可以明确选择本地 adhoc 构建，再把产物作为 GitHub Release 附件上传。此路径不运行正式工作流，也不使用要求稳定证书身份的 `scripts/sign-local-macos-app.sh`。

1. 在干净的待发布提交上确认 Tauri/Cargo 版本一致，执行 `tauri build --target aarch64-apple-darwin --bundles app --no-sign --ci`。
2. 对生成的 App 执行 `codesign --force --deep --sign - <App路径>`，使用 `lipo -archs` 确认只有 `arm64`，并执行 `codesign --verify --deep --strict`。
3. 创建独立 staging 目录，使用 `ditto` 复制 App 并建立 `Applications → /Applications` 软链，再用 `hdiutil create -srcfolder <staging目录> -format UDZO <DMG路径>` 打包。不要直接把 App 本身作为 `-srcfolder`，否则卷根会变成 App 的内部内容。
4. 验证 DMG，使用只读挂载检查卷根 App 和签名后卸载；可用 `ditto -c -k --sequesterRsrc --keepParent` 另生成 ZIP。最后在产物目录内用文件名计算 SHA-256。
5. 创建指向该构建提交 SHA 的版本标签，上传安装包与校验文件；发布说明记录提交 SHA、架构和签名状态。

本地 fallback 的发布说明必须明确写为“**adhoc 签名，未经 Apple 公证，仅支持 Apple Silicon/arm64**”，不得沿用正式工作流的 Developer ID/已公证描述。adhoc 签名不能提供稳定的发布者身份；替换安装后，macOS 可能需要重新授权辅助功能和输入监控。完成 `codesign --verify` 也不等同于通过 Apple 公证或 Gatekeeper 检查。
