# SignPath 代码签名接入指南

目标：消掉安装包的 SmartScreen「未知发布者」警告。SignPath 对开源项目
免费（[Open Source Code Signing](https://signpath.io/open-source) 计划，
证书由 [SignPath Foundation](https://signpath.org) 签发）——Ghost-Downloader-3
用的就是这一条。

## 一次性申请（人工步骤）

1. 在 [signpath.io](https://signpath.io) 注册账号并创建 Organization（免费档即可）。
2. 以项目名义申请 Open Source Code Signing：需要证明仓库归属
   （GitHub URL）与项目活跃度。批准后 SignPath Foundation 发放代码签名
   证书，私钥托管在 SignPath 的硬件里，**任何人拿不到私钥本体**。
3. Organization 里创建 **Project**（slug 建议就叫 `veloxgrab`）与
   **Signing Policy**（slug 建议 `release-signing`；策略上把「允许的
   origin/commit」绑定到本仓库，防止别的构建借用你的签名）。
4. 在项目的 **Trusted Build System** 设置里选 **GitHub.com**（预定义项），
   关联本仓库。
5. 生成一个有 **submitter** 权限的 API Token。
6. 仓库 Settings → Secrets and variables → Actions，添加四个 secret：

   | Secret | 值 |
   |---|---|
   | `SIGNPATH_API_TOKEN` | 第 5 步的 API Token |
   | `SIGNPATH_ORGANIZATION_ID` | Organization 的 id（Org 设置页可见）|
   | `SIGNPATH_PROJECT_SLUG` | `veloxgrab` |
   | `SIGNPATH_SIGNING_POLICY_SLUG` | `release-signing` |

## 之后

`.github/workflows/release.yml` 在 tag 推送时自动：
构建安装包 →（配置了 secrets 才有这步）提交 SignPath 签名 → 用签名版
替换 → 建草稿 Release 上传。未配置时跳过签名，一切照旧。

## 为什么签名值这个麻烦

- SmartScreen 对未签名 exe 的警告是**转化率杀手**：新用户看到「Windows
  已保护你的电脑」直接流失。
- 增量更新器（`update_now_patch`）信任链的最后一环也受益：更新器校验
  的是发布摘要，签名保证的是「Release 上的安装包确实是这个仓库构建的」，
  两层合起来才是完整的供应链。
- 缺点：签名要排队（Foundation 的共享证书通常几分钟），Release 流程会
  慢几分钟——可接受。

## 验证签名

装完后右键 `veloxgrab.exe` → 属性 → 数字签名，应看到 SignPath Foundation
的证书链。命令行：`signtool verify /pa /all VeloxGrab_*_x64-setup.exe`。
