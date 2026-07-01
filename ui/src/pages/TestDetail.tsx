import { useEffect, useState } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getTestHistory } from '../api/bes';
import type { TestExecution } from '../types';

function formatTime(iso?: string) {
  if (!iso) return '-';
  return new Date(iso).toLocaleString();
}

function statusClass(status: string) {
  switch (status) {
    case 'PASSED':
      return 'bg-green-100 text-green-800';
    case 'FAILED':
    case 'TIMEOUT':
    case 'INCOMPLETE':
    case 'REMOTE_FAILURE':
    case 'FAILED_TO_BUILD':
    case 'TOOL_HALTED_BEFORE_TESTING':
      return 'bg-red-100 text-red-800';
    case 'FLAKY':
      return 'bg-yellow-100 text-yellow-800';
    default:
      return 'bg-gray-100 text-gray-800';
  }
}

export default function TestDetail() {
  const { label } = useParams<{ label: string }>();
  const [history, setHistory] = useState<TestExecution[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    async function load() {
      if (!label) return;
      try {
        setLoading(true);
        const data = await getTestHistory(label);
        setHistory(data);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load test history');
      } finally {
        setLoading(false);
      }
    }
    load();
  }, [label]);

  if (loading) return <p className="text-gray-600">Loading test history...</p>;
  if (error) return <p className="text-red-600">{error}</p>;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold text-gray-900">Test History</h2>
          <p className="text-sm text-gray-500 break-all">{label}</p>
        </div>
        <Link
          to="/tests"
          className="text-sm font-medium text-blue-600 hover:text-blue-800"
        >
          &larr; Back to tests
        </Link>
      </div>

      <div className="overflow-hidden rounded-lg border bg-white shadow-sm">
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
                Cached
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Start Time
              </th>
              <th className="px-4 py-3 text-left text-xs font-medium uppercase tracking-wider text-gray-500">
                Duration
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {history.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-6 text-center text-gray-500">
                  No history for this test.
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
                    {ex.invocation_id}
                  </Link>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm">
                  <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${statusClass(ex.status)}`}>
                    {ex.status}
                  </span>
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {ex.cached_locally || ex.cached_remotely ? 'Yes' : 'No'}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {formatTime(ex.start_time)}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-sm text-gray-700">
                  {ex.duration_ms}ms
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
