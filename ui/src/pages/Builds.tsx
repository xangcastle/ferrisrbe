import { useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { getBuilds } from '../api/bes';
import StatusBadge from '../components/StatusBadge';
import type { BuildSummary } from '../types';

function formatDuration(ms?: number) {
  if (ms === undefined || ms === null) return '-';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function cacheHitRatio(build: BuildSummary) {
  if (build.total_actions === 0) return '0%';
  return `${Math.round((build.cached_actions / build.total_actions) * 100)}%`;
}

export default function Builds() {
  const [builds, setBuilds] = useState<BuildSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();

  useEffect(() => {
    async function load() {
      try {
        setLoading(true);
        const data = await getBuilds();
        setBuilds(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load builds');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, []);

  if (loading) return <p className="text-gray-600">Loading builds...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-4">
      <h2 className="text-2xl font-bold text-gray-900">Builds</h2>
      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Invocation ID
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Command
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Status
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Duration
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Actions
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Cache Hit Ratio
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {builds.length === 0 && (
              <tr>
                <td colSpan={6} className="px-4 py-6 text-center text-gray-500">
                  No builds found.
                </td>
              </tr>
            )}
            {builds.map((build) => (
              <tr
                key={build.invocation_id}
                onClick={() => navigate(`/builds/${encodeURIComponent(build.invocation_id)}`)}
                className="cursor-pointer hover:bg-gray-50"
              >
                <td className="whitespace-nowrap px-4 py-3 text-sm font-medium text-gray-900">
                  {build.invocation_id}
                </td>
                <td className="px-4 py-3 text-sm text-gray-700">{build.command}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm">
                  <StatusBadge status={build.status} />
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {formatDuration(build.duration_ms)}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {build.total_actions}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {cacheHitRatio(build)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
