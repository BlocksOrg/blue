export const BLUE_ASCII = String.raw` ____  _
| __ )| |_   _  ___
|  _ \| | | | |/ _ \
| |_) | | |_| |  __/
|____/|_|\__,_|\___|`;

export function dashboardStartupBanner(version: string) {
  return `\n${BLUE_ASCII}\n\nBlue dashboard v${version}\n`;
}
