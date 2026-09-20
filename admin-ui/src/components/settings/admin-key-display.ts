/**
 * 登录密钥展示相关的纯逻辑。
 *
 * 抽出来是为了能测——尤其是文案：这一块的说明**曾经是错的**，而错误文案
 * 在肉眼下和正确的长得一模一样。
 */

/** 遮罩用的固定宽度点串。 */
const MASK = '••••••••••••'

/**
 * 遮住登录密钥。
 *
 * 为什么要能看见：桌面版初始化之后会自动登录，用户可能已经不记得自己
 * 设的是什么；而从浏览器或另一台机器连过来时需要它。
 *
 * 为什么**完全**遮住、连一段都不露：管理密钥现在永远是用户自设的密码
 * （F1 之后不再生成随机串）。密码没有「前缀不是秘密」这种结构——
 * 露出 `sk-admin-` 是无害的，露出 `test123` 的前 7 位不是。
 *
 * 为什么遮罩宽度固定：密码的长度本身就是给爆破用的情报。
 *
 * 要核对「是不是同一个」就点旁边的眼睛。那是一次点击的代价，
 * 换掉的是「设置页在别人能看到屏幕的场合被打开」这一整类风险。
 *
 * 这条是真机跑出来的：原来的实现有一句「太短就原样返回」，那是给 32 位
 * 随机串写的理由；换成用户密码之后，它直接把密码打在屏幕上。
 */
export function maskAdminKey(key: string | null | undefined): string {
  const trimmed = (key ?? '').trim()
  if (!trimmed) return '（未登录）'
  return MASK
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
