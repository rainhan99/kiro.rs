import { useState, useEffect, lazy, Suspense } from "react";
import { storage } from "@/lib/storage";
import {
  applyTheme,
  resolveDarkMode,
  type ThemeId,
  type ThemeMode,
  type ThemeSelection,
} from "@/lib/theme";
import { LoginPage } from "@/components/login-page";
import { SetupPage } from "@/components/setup-page";
import { decideEntryScreen } from "@/components/setup-logic";
import { fetchSetupStatus } from "@/api/setup";
import { Toaster } from "@/components/ui/sonner";
import { ConfirmProvider } from "@/components/ui/confirm-dialog";
import { TooltipProvider } from "@/components/ui/tooltip";
import { tabFromHash } from "@/hooks/use-url-state";
import { AppLayout, type TabKey as Tab } from "@/components/layout/app-layout";

const Dashboard = lazy(() =>
  import("@/components/dashboard").then((m) => ({ default: m.Dashboard })),
);
const OverviewPage = lazy(() =>
  import("@/components/overview-page").then((m) => ({
    default: m.OverviewPage,
  })),
);
const ClientKeysPage = lazy(() =>
  import("@/components/client-keys-page").then((m) => ({
    default: m.ClientKeysPage,
  })),
);
const TraceLogPage = lazy(() =>
  import("@/components/trace-log-page").then((m) => ({
    default: m.TraceLogPage,
  })),
);
const GroupsPage = lazy(() =>
  import("@/components/groups-page").then((m) => ({
    default: m.GroupsPage,
  })),
);
const SettingsPage = lazy(() =>
  import("@/components/settings-page").then((m) => ({
    default: m.SettingsPage,
  })),
);


function readTabFromHash(): Tab {
  // 走共享解析：hash 里现在可能带筛选查询串（#/traces?status=error），
  // 直接全等比较会认不出 Tab。
  const h = tabFromHash();
  if (
    h === "credentials" ||
    h === "keys" ||
    h === "groups" ||
    h === "overview" ||
    h === "traces" ||
    h === "settings"
  )
    return h;
  return "overview";
}

interface AppHeaderProps {
  theme: ThemeSelection;
  isDarkMode: boolean;
  tab: Tab;
  onLogout: () => void;
  onSwitchTab: (next: Tab, subKey?: string) => void;
  onSelectPalette: (palette: ThemeId) => void;
  onSelectMode: (mode: ThemeMode) => void;
}

function App() {
  const app = useAppShell();

  // 状态还没探到时先不画任何一屏：闪一下登录页再跳走，看起来像是掉线了。
  if (app.screen === null) return null;

  if (app.screen === "setup") {
    return <SetupApp providedToken={app.providedSetupToken} onDone={app.handleLogin} />;
  }

  if (app.screen === "login") {
    return <LoggedOutApp onLogin={app.handleLogin} />;
  }

  return (
    <LoggedInApp
      theme={app.theme}
      isDarkMode={app.isDarkMode}
      tab={app.tab}
      onLogout={app.handleLogout}
      onSwitchTab={app.switchTab}
      onSelectPalette={app.selectPalette}
      onSelectMode={app.selectMode}
    />
  );
}

/**
 * 宿主（桌面端）注入的一次性口令。
 *
 * 桌面端自己就是打印这串口令的那个进程，没理由让用户再手抄一遍。
 * 浏览器里这个值不存在，初始化页会照常要求粘贴。
 */
function readProvidedSetupToken(): string | null {
  try {
    return localStorage.getItem("kiroSetupToken");
  } catch {
    return null;
  }
}

function useAppShell() {
  const [isLoggedIn, setIsLoggedIn] = useState(false);
  const [initialized, setInitialized] = useState<boolean | null>(null);
  const [probed, setProbed] = useState(false);
  const providedSetupToken = readProvidedSetupToken();
  const [tab, setTab] = useState<Tab>(readTabFromHash);
  const [theme, setTheme] = useState<ThemeSelection>(() => storage.getThemeSelection());
  const [isDarkMode, setIsDarkMode] = useState(() => resolveDarkMode(theme));

  useEffect(() => {
    if (storage.getApiKey()) setIsLoggedIn(true);
  }, []);

  // 先问一句「这个实例被认领过没有」。未初始化时要显示的是初始化页，
  // 而不是一个没有密码可填的登录页。
  useEffect(() => {
    let alive = true;
    fetchSetupStatus().then((value) => {
      if (!alive) return;
      setInitialized(value);
      setProbed(true);
    });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    const onHash = () => setTab(readTabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    storage.setThemeSelection(theme);
    const resolved = resolveDarkMode(theme);
    setIsDarkMode(applyTheme(theme, resolved));
  }, [theme]);

  useEffect(() => {
    if (theme.mode !== "system" || typeof window.matchMedia !== "function") return;

    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onSystemThemeChange = (event: MediaQueryListEvent) => {
      setIsDarkMode(applyTheme(theme, event.matches));
    };
    media.addEventListener("change", onSystemThemeChange);
    return () => media.removeEventListener("change", onSystemThemeChange);
  }, [theme]);

  const switchTab = (next: Tab, subKey?: string) => {
    const targetHash = subKey ? `#/${next}?s=${subKey}` : `#/${next}`;
    if (window.location.hash !== targetHash) {
      window.location.hash = targetHash;
      window.dispatchEvent(new Event("hashchange"));
    }
    setTab(next);
  };

  const handleLogin = () => setIsLoggedIn(true);
  const handleLogout = () => {
    storage.removeApiKey();
    setIsLoggedIn(false);
  };
  const selectPalette = (palette: ThemeId) => {
    setTheme((current) => ({ ...current, palette }));
  };
  const selectMode = (mode: ThemeMode) => {
    setTheme((current) => ({ ...current, mode }));
  };

  return {
    handleLogin,
    handleLogout,
    isLoggedIn,
    providedSetupToken,
    // 探测没回来之前是 null——此时什么都不画，免得闪一下登录页再跳走。
    screen: probed
      ? isLoggedIn && initialized !== false
        ? "console"
        : decideEntryScreen({ initialized, storedKey: storage.getApiKey() })
      : null,
    isDarkMode,
    selectMode,
    selectPalette,
    switchTab,
    tab,
    theme,
  };
}

function SetupApp({
  providedToken,
  onDone,
}: {
  providedToken: string | null;
  onDone: () => void;
}) {
  return (
    <>
      <SetupPage providedToken={providedToken} onDone={onDone} />
      <Toaster position="top-center" />
    </>
  );
}

function LoggedOutApp({ onLogin }: { onLogin: () => void }) {
  return (
    <>
      <LoginPage onLogin={onLogin} />
      <Toaster position="top-center" />
    </>
  );
}

function LoggedInApp({
  theme,
  isDarkMode,
  onLogout,
  onSwitchTab,
  onSelectPalette,
  onSelectMode,
  tab,
}: AppHeaderProps) {
  return (
    <TooltipProvider delayDuration={150}>
      <ConfirmProvider>
        <AppLayout
          currentTab={tab}
          onSelectTab={onSwitchTab}
          theme={theme}
          isDarkMode={isDarkMode}
          onSelectPalette={onSelectPalette}
          onSelectMode={onSelectMode}
          onLogout={onLogout}
        >
          <AppMain tab={tab} onLogout={onLogout} />
        </AppLayout>
        <Toaster position="top-center" />
      </ConfirmProvider>
    </TooltipProvider>
  );
}

function AppMain({ onLogout, tab }: { onLogout: () => void; tab: Tab }) {
  return (
    <Suspense
      fallback={
        <div className="flex h-64 items-center justify-center text-xs font-mono text-muted-foreground">
          Loading Console Module...
        </div>
      }
    >
      {tab === "overview" && <OverviewPage />}
      {tab === "credentials" && <Dashboard onLogout={onLogout} embedded />}
      {tab === "keys" && <ClientKeysPage />}
      {tab === "groups" && <GroupsPage />}
      {tab === "traces" && <TraceLogPage />}
      {tab === "settings" && <SettingsPage />}
    </Suspense>
  );
}

export default App;
