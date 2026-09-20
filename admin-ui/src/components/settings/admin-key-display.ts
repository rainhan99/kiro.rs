/**
 * 登录密钥展示相关的纯逻辑。
 *
 * 抽出来是为了能测——尤其是文案：这一块的说明**曾经是错的**，而错误文案
 * 在肉眼下和正确的长得一模一样。
 */

/**
 * 脱敏显示登录密钥。
 *
 * 为什么要能看见：桌面版免登录后用户从没见过这个密钥，而从浏览器或另一台
 * 机器连过来时需要它。
 *
 * 为什么默认脱敏：设置页常在别人能看到屏幕的场合打开。保留首尾是为了能
 * 核对「是不是同一个」，那是用户真正要做的判断。
 */
export function maskAdminKey(key: string | null | undefined): string {
  const trimmed = (key ?? '').trim()
  if (!trimmed) return '（未登录）'
  // 短到脱不了敏的就原样给出——造出比原文还长的东西没有意义
  if (trimmed.length <= 16) return trimmed
  return `${trimmed.slice(0, 9)}…${trimmed.slice(-7)}`
}

/**
 * 「当前密钥」这一行的文案。
 *
 * 措辞是这里的重点，不是装饰。原来的说明写「同时是 API 的主密钥
 * （config.json 的 apiKey）」「下游客户端需要同步换成新密钥」——两句都是
 * 错的：`update_admin_key` 只改 `adminApiKey`（src/admin/handlers.rs:959-962），
 * 客户端用的是另一个 key（`apiKey`，同步为 id=0 的系统 Key），完全不受影响。
 *
 * 错误文案的代价不是不好看：它会吓得人不敢轮换管理密钥，或者换完之后
 * 以为自己弄坏了下游。
 */
export function currentKeyRow(): {
  label: string
  description: string
  rotateHint: string
} {
  return {
    label: '当前密钥',
    description:
      '管理面板的登录密钥（config.json 的 adminApiKey）。桌面版会自动登录，' +
      '从浏览器或另一台机器访问时需要它。',
    rotateHint:
      '旧密钥立即失效，其它浏览器需要重新登录。不影响客户端 API Key——' +
      '那是另一个密钥，下游调用照常。',
  }
}
