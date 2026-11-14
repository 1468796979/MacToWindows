#!/bin/zsh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
app_path="${1:-$repo_root/target/aarch64-apple-darwin/release/bundle/macos/MacToWindows.app}"

if [[ ! -d "$app_path" ]]; then
  print -u2 "找不到 macOS App: $app_path"
  print -u2 "先运行 cargo tauri build --target aarch64-apple-darwin --bundles app。"
  exit 1
fi

identity="${CODESIGN_IDENTITY:-}"
if [[ -z "$identity" ]]; then
  identity="$(security find-identity -v -p codesigning \
    | sed -n 's/^[[:space:]]*[0-9][0-9]*)[[:space:]]*\(.*\)$/\1/p' \
    | head -n 1)"
fi

if [[ -z "$identity" ]]; then
  print -u2 "没有可用的稳定代码签名身份。"
  print -u2 "请先在钥匙串中创建或安装 Apple Development、Developer ID，或固定的本地代码签名证书。"
  print -u2 "禁止使用 codesign --sign -，否则每次构建都会生成新的 CDHash 并重新触发 TCC 授权。"
  exit 2
fi

print "使用签名身份: $identity"
codesign --force --deep --options runtime --sign "$identity" "$app_path"
codesign --verify --deep --strict --verbose=2 "$app_path"
codesign -dvvv "$app_path" 2>&1 | sed -n '/Identifier=/p;/TeamIdentifier=/p;/CDHash=/p;/Signature=/p'
