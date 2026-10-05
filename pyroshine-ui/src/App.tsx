import { useState } from "react";
import Alert from "@mui/material/Alert";
import Badge from "@mui/material/Badge";
import Box from "@mui/material/Box";
import ButtonBase from "@mui/material/ButtonBase";
import CircularProgress from "@mui/material/CircularProgress";
import IconButton from "@mui/material/IconButton";
import Menu from "@mui/material/Menu";
import MenuItem from "@mui/material/MenuItem";
import ListItemIcon from "@mui/material/ListItemIcon";
import Snackbar from "@mui/material/Snackbar";
import Stack from "@mui/material/Stack";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import DashboardOutlined from "@mui/icons-material/SpaceDashboardOutlined";
import Dashboard from "@mui/icons-material/SpaceDashboard";
import DevicesOutlined from "@mui/icons-material/DevicesOutlined";
import Devices from "@mui/icons-material/Devices";
import TuneOutlined from "@mui/icons-material/TuneOutlined";
import Tune from "@mui/icons-material/Tune";
import MonitorHeartOutlined from "@mui/icons-material/MonitorHeartOutlined";
import MonitorHeart from "@mui/icons-material/MonitorHeart";
import MoreVert from "@mui/icons-material/MoreVert";
import Logout from "@mui/icons-material/Logout";
import { call } from "./api/bridge";
import { useDaemon } from "./state/daemon";
import { useRoute, type Page } from "./state/route";
import { DashboardPage } from "./pages/Dashboard";
import { ClientsPage } from "./pages/Clients";
import { SettingsPage } from "./pages/Settings";
import { DiagnosticsPage } from "./pages/Diagnostics";
import { Unavailable } from "./components/Unavailable";

const destinations: { page: Page; label: string; icon: typeof Dashboard; active: typeof Dashboard }[] = [
  { page: "dashboard", label: "Dashboard", icon: DashboardOutlined, active: Dashboard },
  { page: "clients", label: "Clients", icon: DevicesOutlined, active: Devices },
  { page: "settings", label: "Settings", icon: TuneOutlined, active: Tune },
  { page: "diagnostics", label: "Diagnostics", icon: MonitorHeartOutlined, active: MonitorHeart },
];

function NavigationRail({ page, navigate, pending }: { page: Page; navigate: (to: string) => void; pending: number }) {
  const [menu, setMenu] = useState<HTMLElement | null>(null);
  return (
    <Stack
      component="nav"
      aria-label="Pyroshine"
      sx={{
        width: 96,
        flexShrink: 0,
        alignItems: "center",
        py: 2,
        gap: 1.5,
        bgcolor: "background.paper",
        borderRight: 1,
        borderColor: "divider",
      }}
    >
      <Box component="img" src="/logo.png" alt="" sx={{ width: 44, height: 44, mb: 2 }} />
      {destinations.map(({ page: target, label, icon: Icon, active: Active }) => {
        const selected = target === page;
        const icon = selected ? <Active /> : <Icon />;
        return (
          <ButtonBase
            key={target}
            onClick={() => navigate(target)}
            aria-current={selected ? "page" : undefined}
            sx={{ flexDirection: "column", gap: 0.5, width: 80, borderRadius: 4, py: 0.5 }}
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
              {target === "clients" ? (
                <Badge color="error" badgeContent={pending} invisible={pending === 0}>
                  {icon}
                </Badge>
              ) : (
                icon
              )}
            </Box>
            <Typography variant="caption" sx={{ fontWeight: selected ? 700 : 500, color: selected ? "text.primary" : "text.secondary" }}>
              {label}
            </Typography>
          </ButtonBase>
        );
      })}
      <Box sx={{ flexGrow: 1 }} />
      <Tooltip title="More" placement="right">
        <IconButton onClick={(event) => setMenu(event.currentTarget)} aria-label="More">
          <MoreVert />
        </IconButton>
      </Tooltip>
      <Menu anchorEl={menu} open={menu !== null} onClose={() => setMenu(null)}>
        <MenuItem
          onClick={() => {
            setMenu(null);
            void call("quit");
          }}
        >
          <ListItemIcon>
            <Logout fontSize="small" />
          </ListItemIcon>
          Quit Pyroshine UI (the server keeps running)
        </MenuItem>
      </Menu>
    </Stack>
  );
}

export function App() {
  const [route, navigate] = useRoute();
  const daemon = useDaemon();
  const pending = daemon.pairing?.requests.filter((request) => !request.approved).length ?? 0;

  let content;
  if (!daemon.loaded) {
    content = (
      <Stack sx={{ alignItems: "center", pt: 12 }}>
        <CircularProgress aria-label="Connecting to Pyroshine" />
      </Stack>
    );
  } else if (!daemon.connected) {
    content = <Unavailable page={route.page} />;
  } else {
    switch (route.page) {
      case "clients":
        content = <ClientsPage request={route.params.get("request")} />;
        break;
      case "settings":
        content = <SettingsPage />;
        break;
      case "diagnostics":
        content = <DiagnosticsPage />;
        break;
      default:
        content = <DashboardPage navigate={navigate} />;
    }
  }

  return (
    <Box sx={{ display: "flex", height: "100vh", overflow: "hidden", bgcolor: "background.default" }}>
      <NavigationRail page={route.page} navigate={navigate} pending={pending} />
      <Box component="main" sx={{ flexGrow: 1, overflow: "auto" }}>
        <Box sx={{ maxWidth: 1240, mx: "auto", px: { xs: 2, md: 4 }, py: 3 }}>{content}</Box>
      </Box>
      <Stack sx={{ position: "fixed", bottom: 16, left: 112, gap: 1, zIndex: 1400 }}>
        {daemon.notices.map((notice) => (
          <Snackbar key={notice.id} open autoHideDuration={7000} onClose={() => daemon.dismiss(notice.id)} sx={{ position: "static" }}>
            <Alert severity={notice.severity} variant="filled" onClose={() => daemon.dismiss(notice.id)} sx={{ minWidth: 320 }}>
              {notice.message}
            </Alert>
          </Snackbar>
        ))}
      </Stack>
    </Box>
  );
}
