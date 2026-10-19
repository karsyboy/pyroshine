import { useState } from "react";
import Alert from "@mui/material/Alert";
import AppBar from "@mui/material/AppBar";
import Badge from "@mui/material/Badge";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import ButtonBase from "@mui/material/ButtonBase";
import CircularProgress from "@mui/material/CircularProgress";
import Drawer from "@mui/material/Drawer";
import IconButton from "@mui/material/IconButton";
import List from "@mui/material/List";
import ListItemButton from "@mui/material/ListItemButton";
import ListItemIcon from "@mui/material/ListItemIcon";
import ListItemText from "@mui/material/ListItemText";
import Snackbar from "@mui/material/Snackbar";
import Stack from "@mui/material/Stack";
import Toolbar from "@mui/material/Toolbar";
import Typography from "@mui/material/Typography";
import useMediaQuery from "@mui/material/useMediaQuery";
import { useTheme } from "@mui/material/styles";
import DashboardOutlined from "@mui/icons-material/SpaceDashboardOutlined";
import Dashboard from "@mui/icons-material/SpaceDashboard";
import DevicesOutlined from "@mui/icons-material/DevicesOutlined";
import Devices from "@mui/icons-material/Devices";
import TuneOutlined from "@mui/icons-material/TuneOutlined";
import Tune from "@mui/icons-material/Tune";
import MonitorHeartOutlined from "@mui/icons-material/MonitorHeartOutlined";
import MonitorHeart from "@mui/icons-material/MonitorHeart";
import SportsEsportsOutlined from "@mui/icons-material/SportsEsportsOutlined";
import SportsEsports from "@mui/icons-material/SportsEsports";
import HistoryOutlined from "@mui/icons-material/HistoryOutlined";
import History from "@mui/icons-material/History";
import NewReleasesOutlined from "@mui/icons-material/NewReleasesOutlined";
import NewReleases from "@mui/icons-material/NewReleases";
import BuildOutlined from "@mui/icons-material/BuildOutlined";
import Build from "@mui/icons-material/Build";
import MenuIcon from "@mui/icons-material/Menu";
import { useAuth } from "./state/auth";
import { useDaemon } from "./state/daemon";
import { useRoute, type Page } from "./state/route";
import { DashboardPage } from "./pages/Dashboard";
import { ClientsPage } from "./pages/Clients";
import { LibraryPage } from "./pages/Library";
import { SettingsPage } from "./pages/Settings";
import { DiagnosticsPage } from "./pages/Diagnostics";
import { HistoryPageView } from "./pages/History";
import { UpdatesPage } from "./pages/Updates";
import { MaintenancePage } from "./pages/Maintenance";
import { SignInPage } from "./pages/SignIn";
import { Unavailable } from "./components/Unavailable";

const destinations: { page: Page; label: string; icon: typeof Dashboard; active: typeof Dashboard }[] = [
  { page: "dashboard", label: "Dashboard", icon: DashboardOutlined, active: Dashboard },
  { page: "library", label: "Applications", icon: SportsEsportsOutlined, active: SportsEsports },
  { page: "clients", label: "Clients", icon: DevicesOutlined, active: Devices },
  { page: "history", label: "History", icon: HistoryOutlined, active: History },
  { page: "settings", label: "Settings", icon: TuneOutlined, active: Tune },
  { page: "diagnostics", label: "Diagnostics", icon: MonitorHeartOutlined, active: MonitorHeart },
  { page: "updates", label: "Updates", icon: NewReleasesOutlined, active: NewReleases },
  { page: "maintenance", label: "Maintenance", icon: BuildOutlined, active: Build },
];

const titles = Object.fromEntries(destinations.map(({ page, label }) => [page, label])) as Record<Page, string>;

interface NavigationProps {
  page: Page;
  navigate: (to: string) => void;
  badges: Partial<Record<Page, number>>;
}

function badged(page: Page, badges: NavigationProps["badges"], icon: React.ReactNode) {
  const count = badges[page] ?? 0;
  return count > 0 ? (
    <Badge color="error" badgeContent={count}>
      {icon}
    </Badge>
  ) : (
    icon
  );
}

/// Wide screens: a Material navigation rail.
function NavigationRail({ page, navigate, badges }: NavigationProps) {
  return (
    <Stack
      component="nav"
      aria-label="Pyroshine"
      sx={{
        width: 96,
        flexShrink: 0,
        alignItems: "center",
        py: 2,
        gap: 1,
        bgcolor: "background.paper",
        borderRight: 1,
        borderColor: "divider",
        overflowY: "auto",
      }}
    >
      <Box component="img" src="/logo.png" alt="" sx={{ width: 44, height: 44, mb: 1.5 }} />
      {destinations.map(({ page: target, label, icon: Icon, active: Active }) => {
        const selected = target === page;
        return (
          <ButtonBase
            key={target}
            onClick={() => navigate(target)}
            aria-current={selected ? "page" : undefined}
            sx={{ flexDirection: "column", gap: 0.5, width: 84, borderRadius: 4, py: 0.5, "&.Mui-focusVisible": { outline: 2, outlineColor: "primary.main" } }}
          >
            <Box
              sx={{
                width: 56,
                height: 32,
                borderRadius: 4,
                display: "grid",
                placeItems: "center",
                bgcolor: selected ? "primary.main" : "transparent",
                color: selected ? "primary.contrastText" : "text.secondary",
                transition: "background-color 150ms",
              }}
            >
              {badged(target, badges, selected ? <Active /> : <Icon />)}
            </Box>
            <Typography variant="caption" sx={{ fontWeight: selected ? 700 : 500, color: selected ? "text.primary" : "text.secondary" }}>
              {label}
            </Typography>
          </ButtonBase>
        );
      })}
    </Stack>
  );
}

/// Narrow screens (phones, small tablets): an app bar with a drawer.
function NavigationBar({ page, navigate, badges }: NavigationProps) {
  const [open, setOpen] = useState(false);
  const total = Object.values(badges).reduce((sum, count) => sum + (count ?? 0), 0);
  return (
    <>
      <AppBar position="sticky" color="inherit" elevation={0} sx={{ borderBottom: 1, borderColor: "divider" }}>
        <Toolbar sx={{ gap: 1 }}>
          <IconButton edge="start" aria-label="Open navigation" onClick={() => setOpen(true)}>
            <Badge color="error" variant="dot" invisible={total === 0}>
              <MenuIcon />
            </Badge>
          </IconButton>
          <Box component="img" src="/logo.png" alt="" sx={{ width: 28, height: 28 }} />
          <Typography variant="h6" component="div" noWrap>
            {titles[page]}
          </Typography>
        </Toolbar>
      </AppBar>
      <Drawer open={open} onClose={() => setOpen(false)}>
        <List component="nav" aria-label="Pyroshine" sx={{ width: 260, pt: 2 }}>
          {destinations.map(({ page: target, label, icon: Icon, active: Active }) => (
            <ListItemButton
              key={target}
              selected={target === page}
              onClick={() => {
                setOpen(false);
                navigate(target);
              }}
            >
              <ListItemIcon>{badged(target, badges, target === page ? <Active /> : <Icon />)}</ListItemIcon>
              <ListItemText primary={label} />
            </ListItemButton>
          ))}
        </List>
      </Drawer>
    </>
  );
}

function Content({ page, params, navigate }: { page: Page; params: URLSearchParams; navigate: (to: string) => void }) {
  const daemon = useDaemon();
  if (!daemon.loaded) {
    return (
      <Stack sx={{ alignItems: "center", pt: 12 }}>
        <CircularProgress aria-label="Connecting to Pyroshine" />
      </Stack>
    );
  }
  if (!daemon.connected && !daemon.server) return <Unavailable title={titles[page]} />;
  switch (page) {
    case "clients":
      return <ClientsPage request={params.get("request")} />;
    case "library":
      return <LibraryPage navigate={navigate} />;
    case "history": {
      const session = Number(params.get("session"));
      return <HistoryPageView selected={Number.isInteger(session) && session > 0 ? session : null} navigate={navigate} />;
    }
    case "settings":
      return <SettingsPage />;
    case "diagnostics":
      return <DiagnosticsPage />;
    case "updates":
      return <UpdatesPage />;
    case "maintenance":
      return <MaintenancePage navigate={navigate} />;
    default:
      return <DashboardPage navigate={navigate} />;
  }
}

export function App() {
  const auth = useAuth();
  const [route, navigate] = useRoute();
  const daemon = useDaemon();
  const theme = useTheme();
  const wide = useMediaQuery(theme.breakpoints.up("md"));

  if (!auth.state?.authenticated) return <SignInPage />;

  const pending = daemon.pairing?.requests.filter((request) => !request.approved).length ?? 0;
  const updateWaiting = daemon.update?.update_available && !daemon.update.dismissed ? 1 : 0;
  const badges: Partial<Record<Page, number>> = { clients: pending, updates: updateWaiting };
  // A tab left open across an upgrade runs the old page against the new server.
  const stale = daemon.server && daemon.server.version !== "test" && __APP_VERSION__ !== daemon.server.version;
  const assets = daemon.web?.assets_version;

  return (
    <Box sx={{ display: "flex", flexDirection: wide ? "row" : "column", height: "100vh", overflow: "hidden", bgcolor: "background.default" }}>
      {wide ? (
        <NavigationRail page={route.page} navigate={navigate} badges={badges} />
      ) : (
        <NavigationBar page={route.page} navigate={navigate} badges={badges} />
      )}
      <Box component="main" sx={{ flexGrow: 1, overflow: "auto" }}>
        <Box sx={{ maxWidth: 1240, mx: "auto", px: { xs: 2, md: 4 }, py: { xs: 2, md: 3 } }}>
          {daemon.restarting && (
            <Alert severity="info" icon={<CircularProgress size={18} />} sx={{ mb: 2 }}>
              Pyroshine is restarting. This page reconnects when it is back.
            </Alert>
          )}
          {!daemon.restarting && daemon.reconnecting && daemon.server && (
            <Alert severity="warning" icon={<CircularProgress size={18} />} sx={{ mb: 2 }}>
              Connection to Pyroshine lost; reconnecting. What this page shows may be out of date.
            </Alert>
          )}
          {stale && (
            <Alert
              severity="warning"
              sx={{ mb: 2 }}
              action={
                <Button color="inherit" size="small" onClick={() => window.location.reload()}>
                  Reload
                </Button>
              }
            >
              {assets && assets !== daemon.server?.version
                ? `The installed web interface is for Pyroshine ${assets}, but the server runs ${daemon.server?.version}. Install matching packages.`
                : `This page is from Pyroshine ${__APP_VERSION__}; the server now runs ${daemon.server?.version}. Reload to use the matching page.`}
            </Alert>
          )}
          <Content page={route.page} params={route.params} navigate={navigate} />
        </Box>
      </Box>
      <Stack sx={{ position: "fixed", bottom: 16, left: { xs: 16, md: 112 }, right: { xs: 16, md: "auto" }, gap: 1, zIndex: 1400 }}>
        {daemon.notices.map((notice) => (
          <Snackbar key={notice.id} open autoHideDuration={7000} onClose={() => daemon.dismiss(notice.id)} sx={{ position: "static" }}>
            <Alert severity={notice.severity} variant="filled" onClose={() => daemon.dismiss(notice.id)} sx={{ minWidth: { md: 320 }, width: "100%" }}>
              {notice.message}
            </Alert>
          </Snackbar>
        ))}
      </Stack>
    </Box>
  );
}
