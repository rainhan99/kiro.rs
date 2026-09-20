import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { Eye, EyeOff, Copy, Wand2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { SettingGroup, SettingRow } from '@/components/console/setting-row'
import { storage } from '@/lib/storage'
import { updateAdminKey } from '@/api/credentials'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { maskAdminKey, currentKeyRow } from './admin-key-display'
import { fetchSecurityConfig, setSecurityConfig } from '@/api/setup'
import { Switch } from '@/components/ui/switch'
import { useConfirm } from '@/components/ui/confirm-dialog'

/**
 * 安全分区：管理面板的登录密钥。
 *
 * 这里刻意**不用**即时保存。轮换登录密钥不是调参数，是一次性的凭据替换：旧密钥
 * 立即失效，正在用它调用 /v1/messages 的下游会全部 401。所以流程反过来 ——
 * 先生成、先复制、二次确认，最后才提交。
 *
 * 提交成功后本地存储自动换成新密钥，当前会话不会被踢出登录。
 */
export function SecuritySection() {
  const confirm = useConfirm()
  const current = currentKeyRow()
  const [revealed, setRevealed] = useState(false)
  const [requireAuth, setRequireAuth] = useState<boolean | null>(null)
  const [draft, setDraft] = useState('')
  const [plain, setPlain] = useState(false)
  const [copied, setCopied] = useState(false)
  const [submitting, setSubmitting] = useState(false)

  useEffect(() => {
    let alive = true
    fetchSecurityConfig()
      .then((c) => alive && setRequireAuth(c.requireAuthOnLaunch))
      .catch(() => alive && setRequireAuth(false))
    return () => {
      alive = false
    }
  }, [])

  const toggleRequireAuth = async (next: boolean) => {
    const previous = requireAuth
    setRequireAuth(next) // 先动，失败再回滚——开关滞后一拍比什么都不动更糟
    try {
      await setSecurityConfig(next)
      toast.success(
        next ? '下次启动桌面版时需要重新输入密码' : '桌面版将记住登录状态',
      )
    } catch (err) {
      setRequireAuth(previous)
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  const copyCurrent = async () => {
    const key = storage.getApiKey()
    if (!key) {
      toast.error('本地没有登录密钥')
      return
    }
    try {
      await navigator.clipboard.writeText(key)
      toast.success('当前密钥已复制到剪贴板')
    } catch {
      toast.error('复制失败，请点「显示」后手动选中')
    }
  }

  const copy = async () => {
    if (!draft.trim()) {
      toast.error('先生成或输入密钥再复制')
      return
    }
    try {
      await navigator.clipboard.writeText(draft)
      setCopied(true)
      toast.success('已复制到剪贴板')
    } catch {
      toast.error('复制失败，请手动选中文本')
    }
  }

  const submit = async () => {
    const key = draft.trim()
    if (!key) return
    if (!copied) {
      const ok = await confirm({
        title: '还没复制新密钥',
        description:
          '旧密钥提交后立即失效。新密钥只在这里显示一次，建议先复制保存再继续。',
        confirmText: '仍然继续',
      })
      if (!ok) return
    }
    const ok = await confirm({
      title: '替换登录密钥？',
      description:
        '旧密钥立即失效，其它已登录的浏览器需要重新登录。当前浏览器会自动切到新密钥，不会掉线。'
        + '客户端 API Key 是另一个密钥，下游调用不受影响。',
      confirmText: '替换',
      destructive: true,
    })
    if (!ok) return

    setSubmitting(true)
    try {
      await updateAdminKey({ newKey: key })
      storage.setApiKey(key)
      toast.success('登录密钥已替换，本地已切到新密钥')
      setDraft('')
      setPlain(false)
      setCopied(false)
    } catch (err) {
      toast.error('替换失败：' + extractErrorMessage(err))
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <SettingGroup
      title="登录密钥"
      description={current.description}
    >
      <SettingRow label={current.label} hint="桌面版自动登录，这里是查看与复制的地方">
        <div className="flex flex-wrap items-center gap-1.5">
          <code className="console-num rounded bg-muted px-2 py-1 text-[12.5px]">
            {revealed ? (storage.getApiKey() ?? '（未登录）') : maskAdminKey(storage.getApiKey())}
          </code>
          <Button
            type="button"
            size="icon"
            variant="ghost"
            onClick={() => setRevealed((v) => !v)}
            title={revealed ? '隐藏' : '显示'}
            className="h-7 w-7"
          >
            {revealed ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
          </Button>
          <Button size="sm" variant="outline" onClick={copyCurrent}>
            <Copy className="h-3.5 w-3.5" />
            复制
          </Button>
        </div>
      </SettingRow>

      <SettingRow
        label="每次启动都要验证"
        hint="仅影响桌面版。打开后关掉窗口重开需要重新输入密码——有人走到没锁屏的电脑前也进不去。下次启动生效。"
      >
        <Switch
          checked={requireAuth ?? false}
          disabled={requireAuth === null}
          onCheckedChange={toggleRequireAuth}
        />
      </SettingRow>

      <SettingRow label="替换密钥" hint={current.rotateHint}>
        <div className="flex flex-wrap items-center gap-1.5">
          <div className="relative">
            <Input
              type={plain ? 'text' : 'password'}
              value={draft}
              onChange={(e) => {
                setDraft(e.target.value)
                setCopied(false)
              }}
              placeholder="输入或生成新密钥"
              disabled={submitting}
              spellCheck={false}
              autoComplete="new-password"
              className="console-num h-8 w-[min(18rem,55vw)] pr-9 text-[12.5px]"
            />
            <Button
              type="button"
              size="icon"
              variant="ghost"
              onClick={() => setPlain((v) => !v)}
              title={plain ? '隐藏' : '显示'}
              className="absolute right-0.5 top-0.5 h-7 w-7"
            >
              {plain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
            </Button>
          </div>
          <Button
            size="sm"
            variant="outline"
            disabled={submitting}
            onClick={() => {
              setDraft(generateApiKey('sk-admin-'))
              setPlain(true)
              setCopied(false)
            }}
            title="生成一个 32 位随机密钥"
          >
            <Wand2 className="h-3.5 w-3.5" />
            生成
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={copy}
            disabled={submitting || !draft.trim()}
          >
            <Copy className="h-3.5 w-3.5" />
            {copied ? '已复制' : '复制'}
          </Button>
          <Button
            size="sm"
            onClick={submit}
            disabled={submitting || !draft.trim()}
          >
            {submitting ? '替换中…' : '替换'}
          </Button>
        </div>
      </SettingRow>
    </SettingGroup>
  )
}
