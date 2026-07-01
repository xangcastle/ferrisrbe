import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { getTargets } from '../api/bes';
import StatusBadge from '../components/StatusBadge';
import type { TargetSummary } from '../types';

export default function Targets() {
  const [targets, setTargets] = useState<TargetSummary[]>([]);
  const [filter, setFilter] = useState('');
  const [statusFilter, setStatusFilter] = useState<string>('all');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();

  useEffect(() => {
    async function load() {
      try {
        setLoading(true);
        const data = await getTargets();
        setTargets(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load targets');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, []);

  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    return targets.filter((t) => {
      const matchesText = t.label.toLowerCase().includes(q) || t.target_kind.toLowerCase().includes(q);
      const matchesStatus = statusFilter === 'all' || t.latest_status === statusFilter;
      return matchesText && matchesStatus;
    });
  }, [targets, filter, statusFilter]);

  if (loading) return <p className="text-gray-600">Loading targets...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-4">
      <h2 className="text-2xl font-bold text-gray-900">Targets</h2>
      <p className="text-sm text-gray-500">
        Historial independiente por target. Cada fila muestra el último estado y el conteo de ejecuciones.
      </p>

      <div className="flex flex-col gap-3 sm:flex-row">
        <input
          type="text"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter by label or kind..."
          className="flex-1 rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none focus:ring-1 focus:ring-blue-500"
        />
        <select
          value={statusFilter}
          onChange={(e) => setStatusFilter(e.target.value)}
          className="rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none focus:ring-1 focus:ring-blue-500"
        >
          <option value="all">All statuses</option>
          <option value="success">Success</option>
          <option value="failure">Failure</option>
          <option value="cached">Cached</option>
        </select>
      </div>

      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Label
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Kind
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Latest
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Executions
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Success / Failure / Cached
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Actions (cached / failed)
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Tags
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {filtered.length === 0 && (
              <tr>
                <td colSpan={7} className="px-4 py-6 text-center text-gray-500">
                  No targets found.
                </td>
              </tr>
            )}
            {filtered.map((target) => (
              <tr
                key={target.label}
                onClick={() => navigate(`/targets/${encodeURIComponent(target.label)}`)}
                className="cursor-pointer hover:bg-gray-50"
              >
                <td className="px-4 py-3 text-sm font-medium text-gray-900">
                  <div className="max-w-md truncate" title={target.label}>
                    {target.label}
                  </div>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">{target.target_kind}</td>
                <td className="whitespace-nowrap px-4 py-3 text-sm">
                  <StatusBadge status={target.latest_status} />
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {target.total_executions}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  <span className="text-green-600">{target.success_count}</span>
                  {' / '}
                  <span className="text-red-600">{target.failure_count}</span>
                  {' / '}
                  <span className="text-blue-600">{target.cached_count}</span>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {target.action_count > 0 ? (
                    <>
                      {target.action_count}{' '}
                      <span className="text-blue-600">({target.cached_actions})</span>
                      {' / '}
                      <span className="text-red-600">{target.failed_actions}</span>
                    </>
                  ) : (
                    <span className="text-gray-400">-</span>
                  )}
                </td>
                <td className="px-4 py-3 text-sm text-gray-700">
                  <div className="flex flex-wrap gap-1">
                    {target.tags.slice(0, 3).map((tag) => (
                      <span key={tag} className="rounded-md bg-gray-100 px-2 py-0.5 text-xs text-gray-600">
                        {tag}
                      </span>
                    ))}
                    {target.tags.length > 3 && (
                      <span className="text-xs text-gray-400">+{target.tags.length - 3}</span>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
