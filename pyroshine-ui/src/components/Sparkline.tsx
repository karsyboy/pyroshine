import Box from "@mui/material/Box";

/// Small trend line for one statistic, scaled to its own range.
export function Sparkline({ values, color = "primary.main", height = 40 }: { values: number[]; color?: string; height?: number }) {
  const width = 200;
  if (values.length < 2) {
    return <Box sx={{ height }} />;
  }
  const max = Math.max(...values);
  const min = Math.min(...values);
  const span = max - min || max || 1;
  const points = values.map((value, index) => {
    const x = (index / (values.length - 1)) * width;
    const y = height - 3 - ((value - min) / span) * (height - 6);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });
  return (
    <Box
      component="svg"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      aria-hidden
      sx={{ width: "100%", height, display: "block", color }}
    >
      <polyline points={`0,${height} ${points.join(" ")} ${width},${height}`} fill="currentColor" opacity={0.12} stroke="none" />
      <polyline points={points.join(" ")} fill="none" stroke="currentColor" strokeWidth={1.75} vectorEffect="non-scaling-stroke" />
    </Box>
  );
}
