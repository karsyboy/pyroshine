import { useState } from "react";
import Button, { type ButtonProps } from "@mui/material/Button";
import RestartAlt from "@mui/icons-material/RestartAlt";
import { api, errorMessage } from "../api/client";
import { useDaemon } from "../state/daemon";
import { ConfirmDialog } from "./common";

/// Restarts Pyroshine after a confirmation: the normal shutdown, then a new
/// start that reads the saved configuration. The page follows it through the
/// daemon state (`restarting`) and reconnects by itself.
export function RestartButton({ variant = "outlined", label = "Restart Pyroshine" }: { variant?: ButtonProps["variant"]; label?: string }) {
  const daemon = useDaemon();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const phase = daemon.session?.phase ?? "idle";
  const application = daemon.session?.session?.application.title;

  const restart = async () => {
    setBusy(true);
    try {
      await api.restart();
      daemon.expectRestart();
      setOpen(false);
    } catch (error) {
      daemon.notify("error", errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Button
        variant={variant}
        startIcon={<RestartAlt />}
        disabled={!daemon.connected || daemon.restarting}
        onClick={() => setOpen(true)}
      >
        {daemon.restarting ? "Restarting…" : label}
      </Button>
      <ConfirmDialog
        open={open}
        title="Restart Pyroshine?"
        confirm="Restart"
        destructive={phase !== "idle"}
        busy={busy}
        onConfirm={() => void restart()}
        onClose={() => setOpen(false)}
      >
        {phase !== "idle"
          ? `This ends the running session${application ? ` and closes ${application}` : ""}; save your progress first. `
          : ""}
        Pyroshine stops and starts again with the saved settings. Pairings, settings and history are kept; this page
        reconnects by itself in a few seconds.
      </ConfirmDialog>
    </>
  );
}
