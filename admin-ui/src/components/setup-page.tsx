import { useState } from 'react'
import { KeyRound, ShieldCheck } from 'lucide-react'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { storage } from '@/lib/storage'
import { extractErrorMessage } from '@/lib/utils'
import { performSetup, createSession } from '@/api/setup'
import {
  validateSetupForm,
  type SetupErrors,
  MIN_ADMIN_KEY_LEN,
} from './setup-logic'

interface SetupPageProps {
  /** 宿主提供的一次性口令。桌面端自己就持有它，用户不必手抄。 */
  providedToken?: string | null
  onDone: (adminKey: string) => void
}

/**
 * 首次初始化：把管理权交到用户手里。
 *
 * 一生只出现一次，所以措辞要把「为什么要我做这一步」说清楚——用户刚装完
 * 软件，最不需要的就是一个没头没尾的输入框。
 */
export function SetupPage({ providedToken, onDone }: SetupPageProps) {
  const tokenProvided = Boolean(providedToken?.trim())
  const [token, setToken] = useState('')
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [errors, setErrors] = useState<SetupErrors>({})
  const [submitting, setSubmitting] = useState(false)
  const [failure, setFailure] = useState<string | null>(null)

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (submitting) return

    const form = { token, password, confirm }
    const found = validateSetupForm(form, { tokenProvided })
    setErrors(found)
    if (Object.keys(found).length > 0) return

    setSubmitting(true)
    setFailure(null)
    try {
      const adminKey = password.trim()
      await performSetup({
        setupToken: tokenProvided ? providedToken!.trim() : token.trim(),
        adminKey,
      })
      // 设完立刻换一个会话 token 登录，用户不必再输一遍自己刚设的密码。
      // 存 token 而不是密码——密码只在这一瞬间存在于内存里。
      const session = await createSession(adminKey)
      storage.setApiKey(session.token)
      onDone(adminKey)
    } catch (err) {
      setFailure(extractErrorMessage(err))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-6">
      <form onSubmit={submit} className="w-full max-w-md space-y-5">
        <div className="space-y-2 text-center">
          <ShieldCheck className="mx-auto h-8 w-8 text-muted-foreground" />
          <h1 className="text-lg font-semibold">设置管理密码</h1>
          <p className="text-sm text-muted-foreground">
            这是第一次启动。设一个你自己的密码，之后用它登录管理界面。
          </p>
        </div>

        {!tokenProvided && (
          <div className="space-y-1.5">
            <label className="text-xs text-muted-foreground">
              一次性口令
              <span className="ml-1 opacity-70">
                — 印在服务启动时的控制台输出里
              </span>
            </label>
            <div className="relative">
              <KeyRound className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
              <Input
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="从控制台粘贴"
                spellCheck={false}
                autoComplete="off"
                disabled={submitting}
                className="console-num pl-8"
              />
            </div>
            {errors.token && (
              <p className="text-xs text-destructive">{errors.token}</p>
            )}
          </div>
        )}

        <div className="space-y-1.5">
          <label className="text-xs text-muted-foreground">
            管理密码 — 至少 {MIN_ADMIN_KEY_LEN} 个字符
          </label>
          <Input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
            disabled={submitting}
          />
          {errors.password && (
            <p className="text-xs text-destructive">{errors.password}</p>
          )}
        </div>

        <div className="space-y-1.5">
          <label className="text-xs text-muted-foreground">再输一遍</label>
          <Input
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            autoComplete="new-password"
            disabled={submitting}
          />
          {errors.confirm && (
            <p className="text-xs text-destructive">{errors.confirm}</p>
          )}
        </div>

        {failure && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {failure}
          </p>
        )}

        <Button type="submit" className="w-full" disabled={submitting}>
          {submitting ? '设置中…' : '完成设置'}
        </Button>

        <p className="text-center text-xs text-muted-foreground">
          这个密码只用于登录管理界面。客户端调用 API 用的是另一个 Key，
          设置完成后可在「API Keys」页看到。
        </p>
      </form>
    </div>
  )
}
