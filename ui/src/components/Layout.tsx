import { NavLink, Outlet, useLocation } from "react-router-dom";
import {
  ChevronsUpDownIcon,
  FlaskConicalIcon,
  GlobeIcon,
  HammerIcon,
  LayoutDashboardIcon,
  LogOutIcon,
  ServerIcon,
  TargetIcon,
  ZapIcon,
} from "lucide-react";
import { cn } from "cn";
import { keycloak } from "@/auth/keycloak";
import { useLiveStatus } from "@/hooks/useLive";
import { ThemeToggle } from "@/components/theme";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/shadcn/ui/dropdown-menu";
import { Separator } from "@/shadcn/ui/separator";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
} from "@/shadcn/ui/sidebar";

const NAV = [
  { to: "/", label: "Dashboard", icon: LayoutDashboardIcon, end: true },
  { to: "/builds", label: "Builds", icon: HammerIcon },
  { to: "/targets", label: "Targets", icon: TargetIcon },
  { to: "/tests", label: "Tests", icon: FlaskConicalIcon },
  { to: "/workers", label: "Workers", icon: ServerIcon },
  { to: "/externals", label: "Externals", icon: GlobeIcon },
];

function pageTitle(pathname: string) {
  const match = NAV.filter((n) => n.to !== "/").find((n) =>
    pathname.startsWith(n.to),
  );
  if (match) return match.label;
  if (pathname.startsWith("/misses")) return "Cache misses";
  return "Dashboard";
}

function LiveIndicator() {
  const status = useLiveStatus();
  const color =
    status === "live"
      ? "bg-success"
      : status === "reconnecting"
        ? "bg-destructive"
        : "bg-warning";
  return (
    <span className="inline-flex items-center gap-2 text-xs text-muted-foreground">
      <span className="relative flex size-2">
        {status === "live" && (
          <span
            className={cn(
              "absolute inline-flex size-full animate-ping rounded-full opacity-60",
              color,
            )}
          />
        )}
        <span
          className={cn("relative inline-flex size-2 rounded-full", color)}
        />
      </span>
      {status}
    </span>
  );
}

function UserMenu() {
  if (!keycloak) return null;
  const profile = keycloak.tokenParsed as
    | { preferred_username?: string; email?: string; name?: string }
    | undefined;
  const name = profile?.name ?? profile?.preferred_username ?? "Signed in";
  const initials = name
    .split(/\s+/)
    .map((part) => part[0])
    .join("")
    .slice(0, 2)
    .toUpperCase();
  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg">
              <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-sidebar-accent text-xs font-semibold">
                {initials}
              </span>
              <span className="grid flex-1 text-left text-sm leading-tight">
                <span className="truncate font-medium">{name}</span>
                <span className="truncate text-xs text-muted-foreground">
                  {profile?.email}
                </span>
              </span>
              <ChevronsUpDownIcon className="ml-auto size-4" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent side="right" align="end" className="min-w-56">
            <DropdownMenuLabel className="font-normal text-muted-foreground">
              {profile?.preferred_username}
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={() => keycloak?.logout()}>
              <LogOutIcon /> Log out
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}

export default function Layout() {
  const { pathname } = useLocation();
  return (
    <SidebarProvider>
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton size="lg" asChild>
                <NavLink to="/">
                  <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-sidebar-primary text-sidebar-primary-foreground">
                    <ZapIcon className="size-4" />
                  </span>
                  <span className="grid flex-1 text-left leading-tight">
                    <span className="truncate font-semibold">FerrisRBE</span>
                    <span className="truncate text-xs text-muted-foreground">
                      Remote build execution
                    </span>
                  </span>
                </NavLink>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarHeader>
        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Platform</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {NAV.map(({ to, label, icon: Icon, end }) => (
                  <SidebarMenuItem key={to}>
                    <NavLink to={to} end={end}>
                      {({ isActive }) => (
                        <SidebarMenuButton
                          isActive={isActive}
                          tooltip={label}
                          asChild
                        >
                          <span>
                            <Icon />
                            <span>{label}</span>
                          </span>
                        </SidebarMenuButton>
                      )}
                    </NavLink>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter>
          <UserMenu />
        </SidebarFooter>
      </Sidebar>
      <SidebarInset className="min-w-0">
        <header className="sticky top-0 z-10 flex h-14 shrink-0 items-center gap-2 border-b bg-background/80 px-4 backdrop-blur">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mx-2 h-4!" />
          <h1 className="text-sm font-medium">{pageTitle(pathname)}</h1>
          <div className="ml-auto flex items-center gap-3">
            <LiveIndicator />
            <ThemeToggle />
          </div>
        </header>
        <main className="min-w-0 flex-1 p-4 sm:p-6">
          <Outlet />
        </main>
      </SidebarInset>
    </SidebarProvider>
  );
}
