import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { Eye, EyeOff, Copy, Wand2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { SettingGroup, SettingRow } from '@/components/console/setting-row'
import { storage } from '@/lib/storage'
import { updateAdminKey } from '@/api/credentials'
import { createSession, fetchSessionTtl, setSessionTtl } from '@/api/setup'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { TTL_OPTIONS, describeTtl } from './session-ttl'

/**
 * 安全分区：管理密码与会话。
 *
 * 替换密钥刻意**不用**即时保存。轮换密码不是调参数，是一次性的凭据替换：
 * 旧密码立即失效。所以流程反过来 —— 先生成、先复制、二次确认，最后才提交。
 *
 * 这里**不显示当前密码**：登录后本地存的是会话 token，根本拿不到密码。
 * 那反而更好——密码不进 localStorage。而密码是用户自己在初始化页设的，
 * 本来就知道。
 */
export function SecuritySection() {
  const confirm = useConfirm()
  const [draft, setDraft] = useState('')
  const [plain, setPlain] = useState(false)
  const [copied, setCopied] = useState(false)
  const [submitting, setSubmitting] = useState(false)
  const [ttl, setTtl] = useState<number | null>(null)

  useEffect(() => {
    let alive = true
    fetchSessionTtl()
      .then((hours) => alive && setTtl(hours))
      .catch(() => alive && setTtl(0))
    return () => {
      alive = false
    }
  }, [])

  const changeTtl = async (hours: number) => {
    const previous = ttl
    setTtl(hours) // 先动，失败再回滚——控件滞后一拍比什么都不动更糟
    try {
      await setSessionTtl(hours)
      toast.success(
        hours === 0 ? '会话不再过期' : `会话 ${describeTtl(hours)}无操作后过期`,
      )
    } catch (err) {
      setTtl(previous)
      toast.error('保存失败：' + extractErrorMessage(err))
    }
  }

  const copy = async () => {
    if (!draft.trim()) {
      toast.error('先生成或输入密码再复制')
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
        title: '还没复制新密码',
        description: '旧密码提交后立即失效。建议先复制保存再继续。',
        confirmText: '仍然继续',
      })
      if (!ok) return
    }
    const ok = await confirm({
      title: '替换管理密码？',
      description:
        '旧密码立即失效，其它已登录的浏览器会在会话过期后无法重新登录。'
        + '客户端 API Key 是另一个密钥，下游调用不受影响。',
      confirmText: '替换',
      destructive: true,
    })
    if (!ok) return

    setSubmitting(true)
    try {
      await updateAdminKey({ newKey: key })
      // 换一个用新密码签发的会话，当前标签页不会掉线。
      const session = await createSession(key)
      storage.setApiKey(session.token)
      toast.success('管理密码已替换')
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
      title="管理密码与会话"
      description="登录管理面板用的密码（config.json 的 adminApiKey）。客户端调用 API 用的是另一个 Key。"
    >
      <SettingRow
        label="会话有效期"
        hint="超过这段时间无操作，浏览器需要重新登录。注意：config.json 里的密码始终有效，脚本与既有自动化不受影响——能读到那个文件的人始终进得来。"
      >
        <select
          value={ttl ?? 0}
          disabled={ttl === null || submitting}
          onChange={(e) => changeTtl(Number(e.target.value))}
          className="h-8 rounded-md border bg-background px-2 text-[12.5px]"
        >
          {TTL_OPTIONS.map((o) => (
            <option key={o.hours} value={o.hours}>
              {o.label}
            </option>
          ))}
        </select>
      </SettingRow>

      <SettingRow
        label="替换密码"
        hint="旧密码立即失效。不影响客户端 API Key——那是另一个密钥，下游调用照常。"
      >
        <div className="flex flex-wrap items-center gap-1.5">
          <div className="relative">
            <Input
              type={plain ? 'text' : 'password'}
              value={draft}
              onChange={(e) => {
                setDraft(e.target.value)
                setCopied(false)
              }}
              placeholder="输入或生成新密码"
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
            title="生成一个随机密码"
          >
            <Wand2 className="h-3.5 w-3.5" />
            生成
          </Button>
          <Button size="sm" variant="outline" onClick={copy} disabled={submitting || !draft.trim()}>
            <Copy className="h-3.5 w-3.5" />
            {copied ? '已复制' : '复制'}
          </Button>
          <Button size="sm" onClick={submit} disabled={submitting || !draft.trim()}>
            {submitting ? '替换中…' : '替换'}
          </Button>
        </div>
      </SettingRow>
    </SettingGroup>
  )
}
