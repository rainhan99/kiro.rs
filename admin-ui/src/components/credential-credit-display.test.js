import { describe, expect, test } from 'bun:test'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { CredentialCard } from './credential-card'

const credential = {
  id: 1, priority: 0, disabled: false, failureCount: 0, totalFailureCount: 0,
  isCurrent: false, expiresAt: null, authMethod: 'api_key', hasProfileArn: false,
  email: 'credits@example.test', subscriptionTitle: 'KIRO PRO',
  successCount: 0, lastUsedAt: null, hasProxy: false, refreshFailureCount: 0,
  endpoint: 'ide', metadata: {},
}

const balance = {
  id: 1, subscriptionTitle: 'KIRO PRO', currentUsage: 1270.21,
  usageLimit: 5000, remaining: 3729.79, usagePercentage: 25.4042,
  nextResetAt: null, overageCapable: true, overageEnabled: false,
}

// Render the real views with synthetic data. Server rendering does not run effects
// or issue balance requests; no hooks, API results, or display components are mocked.
function renderCreditView(view, overrides = {}) {
  const client = new QueryClient({
    defaultOptions: { queries: { enabled: false, retry: false } },
  })
  try {
    return renderToStaticMarkup(createElement(QueryClientProvider, { client },
      createElement(CredentialCard, {
        credential, view, balance: { ...balance, ...overrides },
        selected: false, loadingBalance: false, preview: true, dragDisabled: true,
        onToggleSelect: () => {}, onRefreshBalance: () => {},
      }),
    )).replace(/<[^>]*>/g, '')
  } finally {
    client.clear()
  }
}

describe('credential quota uses Kiro credits, not dollars', () => {
  test('card preserves the supplied remaining, used and limit values with credit units', () => {
    const text = renderCreditView('card')
    expect(text).toContain('3,729.79 积分')
    expect(text).toContain('已用: 1,270.21 积分')
    expect(text).toContain('上限: 5,000.00 积分')
    expect(text).toContain('25.4%')
    expect(text).not.toContain('$')
  })

  test('compact list also displays the unconverted credit balance', () => {
    const text = renderCreditView('list')
    expect(text).toContain('3,729.79 积分')
    expect(text).toContain('25%')
    expect(text).not.toContain('$')
  })

  for (const view of ['card', 'list']) {
    test(`${view} preserves the negative balance when credits are overdrawn`, () => {
      const text = renderCreditView(view, {
        currentUsage: 5001.25, remaining: -1.25, usagePercentage: 100.025,
      })
      expect(text).toContain('-1.25 积分')
      expect(text).not.toContain('$')
    })

    test(`${view} displays a zero credit balance without a currency symbol`, () => {
      const text = renderCreditView(view, {
        currentUsage: 5000, remaining: 0, usagePercentage: 100,
      })
      expect(text).toContain('0.00 积分')
      expect(text).not.toContain('$')
    })
  }
})
