import { createTheme } from "@mui/material/styles";

// Material 3 inspired palette seeded from the Pyroshine flame.
export const theme = createTheme({
  cssVariables: { colorSchemeSelector: "media" },
  colorSchemes: {
    light: {
      palette: {
        primary: { main: "#C43E0C", contrastText: "#FFFFFF" },
        secondary: { main: "#8A5100" },
        success: { main: "#2E7D32" },
        warning: { main: "#B26A00" },
        error: { main: "#BA1A1A" },
        info: { main: "#1565C0" },
        background: { default: "#FFF8F6", paper: "#FFFFFF" },
        text: { primary: "#231917", secondary: "#53433F" },
        divider: "rgba(83, 67, 63, 0.16)",
      },
    },
    dark: {
      palette: {
        primary: { main: "#FFB59E", contrastText: "#5F1600" },
        secondary: { main: "#FFB870" },
        success: { main: "#7DD87F" },
        warning: { main: "#FFB74D" },
        error: { main: "#FFB4AB" },
        info: { main: "#9ECAFF" },
        background: { default: "#1A1110", paper: "#271D1B" },
        text: { primary: "#F1DFDA", secondary: "#D8C2BC" },
        divider: "rgba(216, 194, 188, 0.14)",
      },
    },
  },
  shape: { borderRadius: 16 },
  typography: {
    fontFamily: '"Roboto", "Noto Sans", "Helvetica", "Arial", sans-serif',
    h4: { fontWeight: 500, letterSpacing: 0 },
    h5: { fontWeight: 500 },
    h6: { fontWeight: 500 },
    button: { textTransform: "none", fontWeight: 500, letterSpacing: 0.1 },
    overline: { letterSpacing: 1, fontWeight: 500 },
  },
  components: {
    MuiButton: {
      defaultProps: { disableElevation: true },
      styleOverrides: { root: { borderRadius: 20, paddingInline: 20 } },
    },
    MuiCard: {
      defaultProps: { variant: "outlined" },
      styleOverrides: { root: { borderRadius: 20 } },
    },
    MuiChip: { styleOverrides: { root: { borderRadius: 8 } } },
    MuiTextField: { defaultProps: { size: "small", fullWidth: true } },
    MuiSelect: { defaultProps: { size: "small" } },
    MuiTooltip: { defaultProps: { arrow: true, enterDelay: 400 } },
    MuiDialog: { styleOverrides: { paper: { borderRadius: 28 } } },
    MuiAlert: { styleOverrides: { root: { borderRadius: 12 } } },
  },
});
