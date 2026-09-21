import { describe, expect, test } from 'bun:test'
import { SECTIONS } from './settings-page'
import { TABS } from './layout/app-layout'
import { AppMain } from '../App'

/** 桌面端侧边栏里「系统设置」下的子项。 */
const sidebarChildren = () =>
  (TABS.find((tab) => tab.key === 'settings')?.children ?? []).map((child) => child.key)

describe('设置分区的两份列表必须一致', () => {
  /// 分区列表有两份：设置页自己的（窄屏顶部导航）和侧边栏的（桌面端）。
  /// 加一个分区要改两处，漏一处就只在一种屏宽下点得到——「多上游网关」就这么
  /// 漏过一次，桌面端用户根本找不到入口。
  test('设置页的每个分区，侧边栏里都点得到', () => {
    const inPage = SECTIONS.map((s) => s.key).sort()
    const inSidebar = sidebarChildren().sort()
    expect(inSidebar).toEqual(inPage)
  })

  // 标签**不要求**一致：侧边栏有更多横向空间，用「调度策略」这类完整说法；
  // 设置页顶部的标签条要窄，用「调度」。这是既有的设计选择，不是缺陷。
  // 真正要钉住的是 key 一致——那决定点不点得到。

  test('多上游网关确实在列——这是它漏掉过的那一项', () => {
    expect(SECTIONS.map((s) => s.key)).toContain('gateway')
    expect(sidebarChildren()).toContain('gateway')
  })
})

describe('顶层导航的三份列表必须一致', () => {
  /// TabKey 类型、TABS 数组、AppMain 的分发是三处。漏一处的表现是
  /// 「侧边栏有入口，点进去一片空白」——比编译错误难发现得多，
  /// 因为它编译得过。
  test('TABS 里的每个顶层 key，AppMain 都有对应分支', () => {
    const source = AppMain.toString()
    for (const tab of TABS) {
      expect(source).toContain(`"${tab.key}"`)
    }
  })

  test('对话页确实在顶层导航里', () => {
    expect(TABS.map((t) => t.key)).toContain('chat')
  })
})
