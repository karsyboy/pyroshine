import Alert from "@mui/material/Alert";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import CircularProgress from "@mui/material/CircularProgress";
import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";
import CloudOff from "@mui/icons-material/CloudOff";
import { useState } from "react";
import { errorMessage } from "../api/client";
import { useDaemon } from "../state/daemon";
import { CopyableCommand, PageHeader } from "./common";

/// Shown while the page has lost its live connection to Pyroshine: the
/// service stopped or restarts, or the network between this browser and the
/// host is down. The page reconnects on its own; nothing here starts or
/// stops the service.
export function Unavailable({ title }: { title: string }) {
  const daemon = useDaemon();
  const [error, setError] = useState<string | null>(null);
  return (
    <>
      <PageHeader title={title} />
      <Card>
        <CardContent sx={{ p: { xs: 2.5, sm: 4 } }}>
          <Stack sx={{ gap: 2, alignItems: "flex-start" }}>
            <CloudOff color="disabled" sx={{ fontSize: 48 }} />
            <Typography variant="h5">Pyroshine can't be reached</Typography>
            <Stack direction="row" sx={{ gap: 1, alignItems: "center", color: "text.secondary" }}>
              {daemon.reconnecting && <CircularProgress size={16} aria-label="Reconnecting" />}
              <Typography color="text.secondary" sx={{ maxWidth: 640 }}>
                This page reconnects automatically as soon as the service answers again. A stream that is running is not
                affected by this page.
              </Typography>
            </Stack>
            <Typography color="text.secondary" sx={{ maxWidth: 640 }}>
              On the host, check or start the service:
            </Typography>
            <Stack sx={{ gap: 1, width: "100%", maxWidth: 560 }}>
              <CopyableCommand command='systemctl status "pyroshine@$USER"' />
              <CopyableCommand command='sudo systemctl start "pyroshine@$USER"' />
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ maxWidth: 640 }}>
              Opened from another computer? The SSH tunnel or VPN to the host must be up as well.
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
