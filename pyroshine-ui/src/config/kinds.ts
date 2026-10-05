import type { FieldKind } from "../api/types";

/// Field kinds the settings editor has a control for (see FieldControl).
export const renderedKinds: FieldKind["type"][] = [
  "text",
  "path",
  "bool",
  "integer",
  "number",
  "port",
  "choice",
  "command",
  "command_list",
  "path_list",
  "applications",
  "scanners",
];
