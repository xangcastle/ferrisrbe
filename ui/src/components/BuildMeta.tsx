import type { BuildSummary } from "../types";

export function invocationTitle(build: BuildSummary) {
  const pattern = build.pattern?.length ? ` ${build.pattern.join(" ")}` : "";
  return `${build.command}${pattern}`;
}

export function mergeRequestUrl(meta: Record<string, string>) {
  if (meta.PR_URL) return meta.PR_URL;
  if (meta.REPO_URL && meta.PR_NUMBER)
    return `${meta.REPO_URL}/-/merge_requests/${meta.PR_NUMBER}`;
  return undefined;
}

export function commitUrl(meta: Record<string, string>) {
  if (meta.REPO_URL && meta.COMMIT_SHA)
    return `${meta.REPO_URL}/-/commit/${meta.COMMIT_SHA}`;
  return undefined;
}

function ExternalLink({
  href,
  children,
}: {
  href?: string;
  children: React.ReactNode;
}) {
  if (!href) return <>{children}</>;
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="text-primary hover:underline"
    >
      {children}
    </a>
  );
}

/** One-line origin: who, which MR, which branch. Empty when no CI metadata. */
export function BuildOrigin({ build }: { build: BuildSummary }) {
  const meta = build.metadata ?? {};
  const user = meta.USER || meta.USER_LOGIN;
  const branch = meta.PR_SOURCE_BRANCH_NAME || meta.BRANCH_NAME;
  if (!user && !meta.PR_NUMBER && !branch) return null;
  return (
    <span className="inline-flex flex-wrap items-center gap-x-2 text-sm text-foreground/80">
      {user && <span>{user}</span>}
      {meta.PR_NUMBER && (
        <ExternalLink href={mergeRequestUrl(meta)}>
          !{meta.PR_NUMBER}
        </ExternalLink>
      )}
      {branch && (
        <code className="rounded bg-muted px-1 py-0.5 text-xs">{branch}</code>
      )}
    </span>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <dt className="text-sm font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-1 text-sm text-foreground">{children}</dd>
    </div>
  );
}

/** CI provenance card: user, MR, branches, commit, job and pipeline links. */
export function BuildOriginCard({ build }: { build: BuildSummary }) {
  const meta = build.metadata ?? {};
  const known = new Set([
    "USER",
    "USER_LOGIN",
    "PR_NUMBER",
    "PR_ID",
    "PR_URL",
    "PR_SOURCE_BRANCH_NAME",
    "PR_TARGET_BRANCH_NAME",
    "BRANCH_NAME",
    "COMMIT_SHA",
    "COMMIT_MESSAGE",
    "COMMIT_AUTHOR",
    "COMMIT_AUTHOR_NAME",
    "COMMIT_AUTHOR_EMAIL",
    "COMMIT_TIMESTAMP",
    "BUILD_URL",
    "PIPELINE_URL",
    "REPO_URL",
    "REPO_OWNER",
    "REPO_NAME",
    "VCS",
    "CI_HOST",
    "RUN_TYPE",
    "ASPECT_TASK_NAME",
    "ASPECT_TASK_ID",
    "ROLE",
  ]);
  const extra = Object.entries(meta).filter(([k]) => !known.has(k));
  if (Object.keys(meta).length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        No CI metadata. Pass{" "}
        <code className="rounded bg-muted px-1 py-0.5 text-xs">
          --build_metadata=KEY=VALUE
        </code>{" "}
        to attach it.
      </p>
    );
  }
  const branch = meta.PR_SOURCE_BRANCH_NAME || meta.BRANCH_NAME;
  return (
    <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {(meta.USER || meta.USER_LOGIN) && (
        <Field label="User">
          {meta.USER}
          {meta.USER_LOGIN && (
            <span className="text-muted-foreground"> @{meta.USER_LOGIN}</span>
          )}
        </Field>
      )}
      {meta.PR_NUMBER && (
        <Field label="Merge request">
          <ExternalLink href={mergeRequestUrl(meta)}>
            !{meta.PR_NUMBER}
          </ExternalLink>
          {meta.PR_TARGET_BRANCH_NAME && (
            <span className="text-muted-foreground">
              {" "}
              → {meta.PR_TARGET_BRANCH_NAME}
            </span>
          )}
        </Field>
      )}
      {branch && (
        <Field label="Branch">
          <code className="rounded bg-muted px-1 py-0.5 text-xs">{branch}</code>
        </Field>
      )}
      {meta.COMMIT_SHA && (
        <Field label="Commit">
          <ExternalLink href={commitUrl(meta)}>
            <code className="text-xs">{meta.COMMIT_SHA.slice(0, 10)}</code>
          </ExternalLink>
          {meta.COMMIT_MESSAGE && (
            <span
              className="block truncate text-muted-foreground"
              title={meta.COMMIT_MESSAGE}
            >
              {meta.COMMIT_MESSAGE.split("\n")[0]}
            </span>
          )}
        </Field>
      )}
      {(meta.BUILD_URL || meta.PIPELINE_URL) && (
        <Field label="CI">
          {meta.BUILD_URL && (
            <ExternalLink href={meta.BUILD_URL}>Job</ExternalLink>
          )}
          {meta.BUILD_URL && meta.PIPELINE_URL && " · "}
          {meta.PIPELINE_URL && (
            <ExternalLink href={meta.PIPELINE_URL}>Pipeline</ExternalLink>
          )}
          {meta.ASPECT_TASK_NAME && (
            <span className="text-muted-foreground">
              {" "}
              · {meta.ASPECT_TASK_NAME}
            </span>
          )}
        </Field>
      )}
      {(meta.ROLE || meta.RUN_TYPE) && (
        <Field label="Run">
          {[meta.ROLE, meta.RUN_TYPE].filter(Boolean).join(" · ")}
        </Field>
      )}
      {extra.map(([k, v]) => (
        <Field key={k} label={k}>
          <span className="break-all">{v}</span>
        </Field>
      ))}
    </dl>
  );
}
