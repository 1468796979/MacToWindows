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

打开 GitHub Actions → **Release macOS arm64** → **Run workflow**，输入一个新的 tag，例如 `v0.1.2`。工作流成功后，下载者可以直接安装经过 Developer ID 签名并完成 Apple 公证的 DMG。

GitHub Actions 使用临时 keychain，构建完成后会删除，不会把证书或 API key 写入产物。
