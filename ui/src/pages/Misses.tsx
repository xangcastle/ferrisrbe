import { useEffect, useMemo, useState } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getMisses } from '../api/bes';
import type { ActionExecutedEvent } from '../types';

export default function Misses() {
  const { id } = useParams<{ id: string }>();
  const [misses, setMisses] = useState<ActionExecutedEvent[]>([]);
  const [filter, setFilter] = useState('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    async function load() {
      try {
        setLoading(true);
        const data = await getMisses(id!);
        setMisses(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load misses');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, [id]);

  const filtered = useMemo(() => {
    const q = filter.toLowerCase();
    return misses.filter(
      (m) =>
        m.label.toLowerCase().includes(q) || m.type.toLowerCase().includes(q)
    );
  }, [misses, filter]);

  if (loading) return <p className="text-gray-600">Loading misses...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold text-gray-900">Cache Misses</h2>
          <p className="text-sm text-gray-500">Build: {id}</p>
        </div>
        <Link
          to={`/builds/${encodeURIComponent(id!)}`}
          className="text-sm font-medium text-blue-600 hover:text-blue-800"
        >
          &larr; Back to build
        </Link>
      </div>

      <input
        type="text"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="Filter by label or type..."
        className="w-full rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none focus:ring-1 focus:ring-blue-500 sm:w-96"
      />

      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Label
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Type
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Remote Cache Hit
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Exit Code
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Duration
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {filtered.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-6 text-center text-gray-500">
                  No misses found.
                </td>
              </tr>
            )}
            {filtered.map((miss, idx) => (
              <tr key={`${miss.label}-${idx}`} className="hover:bg-gray-50">
                <td className="px-4 py-3 text-sm font-medium text-gray-900">{miss.label}</td>
                <td className="px-4 py-3 text-sm text-gray-700">{miss.type}</td>
                <td className="px-4 py-3 text-sm text-gray-700">
                  {miss.remote_cache_hit ? 'Yes' : 'No'}
                </td>
                <td className="px-4 py-3 text-sm text-gray-700">{miss.exit_code ?? '-'}</td>
                <td className="px-4 py-3 text-sm text-gray-700">
                  {miss.duration_ms !== undefined ? `${miss.duration_ms}ms` : '-'}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
