import { useEffect, useMemo, useState } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getTargetHistory } from '../api/bes';
import StatusBadge from '../components/StatusBadge';
import type { TargetExecution } from '../types';

function formatTime(iso?: string) {
  if (!iso) return '-';
  return new Date(iso).toLocaleString();
}

function formatDuration(ms?: number) {
  if (ms === undefined || ms === null) return '-';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function BuildTimeline({ history }: { history: TargetExecution[] }) {
  const maxDuration = useMemo(() => {
    return Math.max(...history.map((h) => h.action_duration_ms || h.build_duration_ms), 1);
  }, [history]);

  return (
    <div className="space-y-2">
      {history.map((ex, idx) => {
        const duration = ex.action_duration_ms || ex.build_duration_ms;
        const widthPercent = maxDuration > 0 ? (duration / maxDuration) * 100 : 0;
        return (
          <div key={idx} className="flex items-center gap-3 text-sm">
            <Link
              to={`/builds/${encodeURIComponent(ex.invocation_id)}`}
              className="w-48 shrink-0 truncate text-blue-600 hover:text-blue-800"
              title={ex.invocation_id}
            >
              {ex.invocation_id.slice(0, 8)}...
            </Link>
            <div className="flex-1">
              <div className="h-5 rounded bg-gray-100">
                <div
                  className={`h-5 rounded ${
                    ex.status === 'failure' ? 'bg-red-500' : ex.status === 'cached' ? 'bg-blue-500' : 'bg-green-500'
                  }`}
                  style={{ width: `${Math.max(widthPercent, 2)}%` }}
                  title={`${formatDuration(duration)}`}
                />
              </div>
            </div>
            <span className="w-20 shrink-0 text-right text-gray-700">{formatDuration(duration)}</span>
          </div>
        );
      })}
    </div>
  );
}

export default function TargetDetail() {
  const { label } = useParams<{ label: string }>();
  const [history, setHistory] = useState<TargetExecution[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    async function load() {
      if (!label) return;
      try {
        setLoading(true);
        const data = await getTargetHistory(label);
        setHistory(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load target history');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, [label]);

  const latestEnvVars = useMemo(() => {
    return history[history.length - 1]?.env_vars ?? {};
  }, [history]);

  if (loading) return <p className="text-gray-600">Loading target history...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold text-gray-900">Target History</h2>
          <p className="text-sm text-gray-500 break-all">{label}</p>
        </div>
        <Link to="/targets" className="text-sm font-medium text-blue-600 hover:text-blue-800">
          &larr; Back to targets
        </Link>
      </div>

      <div className="rounded-lg border bg-white p-5 shadow-sm">
        <h3 className="mb-4 text-lg font-semibold text-gray-900">Build Duration Timeline</h3>
        {history.length === 0 ? (
          <p className="text-gray-500">No history available.</p>
        ) : (
          <BuildTimeline history={history} />
        )}
        <p className="mt-3 text-xs text-gray-500">
          Bars show action duration when available; otherwise full build duration. Use{' '}
          <code className="rounded bg-gray-100 px-1 py-0.5">--build_event_publish_all_actions=true</code>{' '}
          for action-level timing.
        </p>
      </div>

      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
        <div className="border-b px-4 py-3">
          <h3 className="text-lg font-semibold text-gray-900">Execution History</h3>
        </div>
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Build
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Status
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Actions
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Cached
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Failed
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Action Duration
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Build Duration
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Build Time
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {history.length === 0 && (
              <tr>
                <td colSpan={8} className="px-4 py-6 text-center text-gray-500">
                  No history for this target.
                </td>
              </tr>
            )}
            {history.map((ex, idx) => (
              <tr key={idx} className="hover:bg-gray-50">
                <td className="px-4 py-3 text-sm font-medium text-gray-900">
                  <Link
                    to={`/builds/${encodeURIComponent(ex.invocation_id)}`}
                    className="text-blue-600 hover:text-blue-800"
                  >
                    {ex.invocation_id.slice(0, 16)}...
                  </Link>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm">
                  <StatusBadge status={ex.status} />
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">{ex.action_count}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">{ex.cached_actions}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">{ex.failed_actions}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {formatDuration(ex.action_duration_ms)}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {formatDuration(ex.build_duration_ms)}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {formatTime(ex.build_start_time)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {Object.keys(latestEnvVars).length > 0 && (
        <div className="rounded-lg border bg-white p-5 shadow-sm">
          <h3 className="mb-3 text-lg font-semibold text-gray-900">
            Environment Variables (latest build)
          </h3>
          <div className="max-h-64 overflow-auto rounded border">
            <table className="min-w-full divide-y divide-gray-200">
              <thead className="bg-gray-50 sticky top-0">
                <tr>
                  <th className="px-4 py-2 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Variable
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                    Value
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-200">
                {Object.entries(latestEnvVars).map(([key, value]) => (
                  <tr key={key}>
                    <td className="px-4 py-2 text-sm font-medium text-gray-900">{key}</td>
                    <td className="px-4 py-2 text-sm text-gray-700 break-all font-mono">{value}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
