import type { LucideIcon } from "lucide-react";
import { SearchIcon } from "lucide-react";
import { cn } from "cn";
import { Input } from "@/shadcn/ui/input";

/** Title row of a page: icon tile, title, one-line description, actions. */
export function PageHeader({
  icon: Icon,
  title,
  description,
  children,
}: {
  icon: LucideIcon;
  title: React.ReactNode;
  description?: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex min-w-0 items-center gap-3">
        <span className="flex size-10 shrink-0 items-center justify-center rounded-xl bg-primary/10 text-primary ring-1 ring-primary/15">
          <Icon className="size-5" />
        </span>
        <div className="min-w-0">
          <h2 className="truncate text-2xl font-semibold tracking-tight">
            {title}
          </h2>
          {description && (
            <p className="truncate text-sm text-muted-foreground">
              {description}
            </p>
          )}
        </div>
      </div>
      {children && (
        <div className="flex shrink-0 flex-wrap items-center gap-2">
          {children}
        </div>
      )}
    </div>
  );
}

export function SearchInput({
  value,
  onChange,
  placeholder,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  className?: string;
}) {
  return (
    <div className={cn("relative w-full sm:w-72", className)}>
      <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
      <Input
        type="search"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="pl-8"
      />
    </div>
  );
}

/** Centered icon, title and hint for a list or panel with nothing in it. */
export function EmptyState({
  icon: Icon,
  title,
  children,
  className,
}: {
  icon: LucideIcon;
  title: string;
  children?: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-col items-center gap-2 px-6 py-10 text-center",
        className,
      )}
    >
      <span className="flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
        <Icon className="size-5" />
      </span>
      <p className="font-medium">{title}</p>
      {children && (
        <p className="max-w-md text-sm text-muted-foreground">{children}</p>
      )}
    </div>
  );
}

/** Small key/value figure used in page summary strips. */
export function Stat({
  icon: Icon,
  label,
  value,
}: {
  icon: LucideIcon;
  label: string;
  value: React.ReactNode;
}) {
  return (
    <div className="flex items-center gap-3 rounded-xl border bg-card px-4 py-3 shadow-xs">
      <span className="flex size-8 items-center justify-center rounded-lg bg-muted text-muted-foreground">
        <Icon className="size-4" />
      </span>
      <div className="min-w-0">
        <p className="text-xs text-muted-foreground">{label}</p>
        <p className="truncate font-semibold tabular-nums">{value}</p>
      </div>
    </div>
  );
}

/**
 * Stacked bar of outcome counts (passed, failed, cached...) with the
 * numbers beside it, for one-glance health in a table row.
 */
export function OutcomeBar({
  parts,
}: {
  parts: { label: string; count: number; className: string }[];
}) {
  const total = parts.reduce((sum, p) => sum + p.count, 0);
  if (total === 0) return <span className="text-muted-foreground">–</span>;
  return (
    <div className="flex min-w-32 items-center gap-2">
      <div className="flex h-1.5 flex-1 overflow-hidden rounded-full bg-muted">
        {parts
          .filter((p) => p.count > 0)
          .map((p) => (
            <span
              key={p.label}
              title={`${p.count.toLocaleString()} ${p.label}`}
              className={p.className}
              style={{ width: `${(p.count / total) * 100}%` }}
            />
          ))}
      </div>
      <span className="text-xs text-muted-foreground tabular-nums">
        {parts.map((p) => p.count.toLocaleString()).join(" / ")}
      </span>
    </div>
  );
}
