import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { getStats, getBuilds } from '../api/bes';
import StatusBadge from '../components/StatusBadge';
import type { GlobalStats, BuildSummary } from '../types';

function StatCard({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-lg border bg-white p-5 shadow-sm">
      <p className="text-sm font-medium text-gray-500">{label}</p>
      <p className="mt-1 text-3xl font-semibold text-gray-900">{value.toLocaleString()}</p>
    </div>
  );
}

export default function Dashboard() {
  const [stats, setStats] = useState<GlobalStats | null>(null);
  const [builds, setBuilds] = useState<BuildSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    async function load() {
      try {
        setLoading(true);
        const [statsData, buildsData] = await Promise.all([getStats(), getBuilds()]);
        setStats(statsData);
        setBuilds(buildsData.slice(0, 10));
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load dashboard');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, []);

  if (loading) return <p className="text-gray-600">Loading dashboard...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-6">
      <h2 className="text-2xl font-bold text-gray-900">Dashboard</h2>
      {stats && (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <StatCard label="Total Builds" value={stats.total_builds} />
          <StatCard label="In Progress" value={stats.in_progress_builds} />
          <StatCard label="Successful Builds" value={stats.successful_builds} />
          <StatCard label="Failed Builds" value={stats.failed_builds} />
          <StatCard label="Total Actions" value={stats.total_actions} />
          <StatCard label="Failed Actions" value={stats.failed_actions} />
        </div>
      )}

      <div className="rounded-lg border bg-white shadow-sm">
        <div className="border-b px-4 py-3">
          <h3 className="text-lg font-semibold text-gray-900">Recent Builds</h3>
        </div>
        <ul className="divide-y">
          {builds.length === 0 && (
            <li className="px-4 py-6 text-center text-gray-500">No builds yet.</li>
          )}
          {builds.map((build) => (
            <li key={build.invocation_id} className="flex items-center justify-between px-4 py-3">
              <div>
                <p className="font-medium text-gray-900">{build.command}</p>
                <p className="text-sm text-gray-500">{build.invocation_id}</p>
              </div>
              <div className="flex items-center gap-3">
                <StatusBadge status={build.status} />
                <Link
                  to={`/builds/${encodeURIComponent(build.invocation_id)}`}
                  className="text-sm font-medium text-blue-600 hover:text-blue-800"
                >
                  View
                </Link>
              </div>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
