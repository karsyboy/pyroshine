import Alert from "@mui/material/Alert";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";
import CloudOff from "@mui/icons-material/CloudOff";
import { useState } from "react";
import { errorMessage } from "../api/bridge";
import { useDaemon } from "../state/daemon";
import type { Page } from "../state/route";
import { CopyableCommand, PageHeader } from "./common";

/// Shown while the Pyroshine service is not running or not reachable.
export function Unavailable({ page }: { page: Page }) {
  const daemon = useDaemon();
  const [error, setError] = useState<string | null>(null);
  const titles: Record<Page, string> = {
    dashboard: "Dashboard",
    clients: "Clients",
    settings: "Settings",
    diagnostics: "Diagnostics",
  };
  return (
    <>
      <PageHeader title={titles[page]} />
      <Card>
        <CardContent sx={{ p: 4 }}>
          <Stack sx={{ gap: 2, alignItems: "flex-start" }}>
            <CloudOff color="disabled" sx={{ fontSize: 48 }} />
            <Typography variant="h5">Pyroshine isn't running</Typography>
            <Typography color="text.secondary" sx={{ maxWidth: 640 }}>
              This app manages a Pyroshine service running for your user. It reconnects automatically as soon as the
              service starts; it does not start or stop the service itself. To check or start it:
            </Typography>
            <Stack sx={{ gap: 1, width: "100%", maxWidth: 560 }}>
              <CopyableCommand command='systemctl status "pyroshine@$USER"' />
              <CopyableCommand command='sudo systemctl start "pyroshine@$USER"' />
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ maxWidth: 640 }}>
              If Pyroshine runs but this app can't reach it, both must use the same user's session bus. Pyroshine logs
              why its desktop interface is unavailable.
            </Typography>
            {error && <Alert severity="error">{error}</Alert>}
            <Button
              variant="outlined"
              onClick={() => {
                setError(null);
                daemon.refresh().catch((e) => setError(errorMessage(e)));
              }}
            >
              Try again
            </Button>
          </Stack>
        </CardContent>
      </Card>
    </>
  );
}
