/**
 * 首次初始化的纯逻辑。
 *
 * 抽出来是为了能测——这一屏只在实例一生中出现一次，手工回归的成本是
 * 「删掉配置重来一遍」，而它决定的是用户能不能进门。
 */

/**
 * 管理密码的最短长度。
 *
 * 必须与后端 `src/admin/setup.rs` 的 `MIN_ADMIN_KEY_LEN` 一致。不一致的
 * 表现是前端放行、后端 400，用户看到的是「我明明填对了」。
 */
export const MIN_ADMIN_KEY_LEN = 8

export type SetupForm = {
  token: string
  password: string
  confirm: string
}

export type SetupErrors = Partial<Record<keyof SetupForm, string>>

/**
 * 校验初始化表单。
 *
 * `tokenProvided` 为真表示 token 由宿主提供（桌面端自己就持有它），
 * 表单里根本没有这个字段，不该因为它为空而拦。
 */
export function validateSetupForm(
  form: SetupForm,
  options: { tokenProvided?: boolean } = {},
): SetupErrors {
  const errors: SetupErrors = {}

  if (!options.tokenProvided && !form.token.trim()) {
    errors.token = '请粘贴启动时控制台打印的一次性口令'
  }

  // 按**字符**算不按字节算：「密码密码」是 4 个字符 12 个字节。
  // 与后端同一个口径。
  const length = [...form.password.trim()].length
  if (length === 0) {
    errors.password = '请设置管理密码'
  } else if (length < MIN_ADMIN_KEY_LEN) {
    errors.password = `密码至少 ${MIN_ADMIN_KEY_LEN} 个字符`
  }

  // 两次确认必须拦在前端。这是设密码，打错了当场进不去，
  // 而唯一的补救是重启服务拿一个新 token。
  if (!errors.password && form.password !== form.confirm) {
    errors.confirm = '两次输入不一致'
  }

  return errors
}

/**
 * 应用外壳的状态。
 *
 * 只有这三个字段，屏幕完全由它们决定。**不要**在别处再存一份「是否已
 * 登录」——两个重叠的真相源正是那个「设完密码卡在初始化页」的 bug 的
 * 来源：`initialized` 在挂载时探一次就不再更新，而另一份状态变了。
 */
export type ShellState = {
  /** 初始化状态探测是否已经回来。 */
  probed: boolean
  /** 实例是否已被认领。`null` 表示没探到。 */
  initialized: boolean | null
  /** 本地是否存着可用的管理密钥。 */
  hasKey: boolean
}

export type Screen = 'loading' | 'setup' | 'login' | 'console'

/**
 * 该看到哪一屏。这是唯一的决策点。
 *
 * `initialized === null` 表示状态**没探到**（服务刚起来、网络抖动）。
 * 这时不能猜「未初始化」——那会把初始化页摆到一个已经配好的实例的用户
 * 脸上，看起来像是配置丢了。保守回落：有密钥就照常进，没有就登录页。
 */
export function decideEntryScreen(state: ShellState): Screen {
  if (!state.probed) return 'loading'
  if (state.initialized === false) return 'setup'
  if (state.hasKey) return 'console'
  return 'login'
}

/**
 * 初始化完成。
 *
 * 两件事同时发生，缺一不可：实例被认领了（`initialized`），而且认领它
 * 的人现在也登录了（`hasKey`）。只更新后者就会卡在初始化页——那正是
 * 真机跑出来的那个 bug。
 */
export function applySetupCompleted(state: ShellState): ShellState {
  return { ...state, initialized: true, hasKey: true }
}

/** 登录成功。 */
export function applyLoggedIn(state: ShellState): ShellState {
  return { ...state, hasKey: true }
}

/** 登出。回到登录页而不是初始化页——实例已经被认领了。 */
export function applyLoggedOut(state: ShellState): ShellState {
  return { ...state, hasKey: false }
}
