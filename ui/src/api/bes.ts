import axios from 'axios';
import type {
  BuildSummary,
  GlobalStats,
  BuildEvent,
  ActionExecutedEvent,
  TargetSummary,
  TargetExecution,
  TestSummary,
  TestExecution,
} from '../types';

const api = axios.create({
  baseURL: '/api',
  headers: {
    'Content-Type': 'application/json',
  },
});

export async function getStats(): Promise<GlobalStats> {
  const { data } = await api.get<GlobalStats>('/stats');
  return data;
}

export async function getBuilds(): Promise<BuildSummary[]> {
  const { data } = await api.get<BuildSummary[]>('/builds');
  return data;
}

export async function getBuild(id: string): Promise<BuildSummary> {
  const { data } = await api.get<BuildSummary>(`/builds/${encodeURIComponent(id)}`);
  return data;
}

export async function getEvents(id: string): Promise<BuildEvent[]> {
  const { data } = await api.get<BuildEvent[]>(`/builds/${encodeURIComponent(id)}/events`);
  return data;
}

export async function getMisses(id: string): Promise<ActionExecutedEvent[]> {
  const { data } = await api.get<ActionExecutedEvent[]>(`/builds/${encodeURIComponent(id)}/misses`);
  return data;
}

export async function getBuildTargets(id: string): Promise<TargetExecution[]> {
  const { data } = await api.get<TargetExecution[]>(`/builds/${encodeURIComponent(id)}/targets`);
  return data;
}

export async function getTargets(): Promise<TargetSummary[]> {
  const { data } = await api.get<TargetSummary[]>('/targets');
  return data;
}

export async function getTargetHistory(label: string): Promise<TargetExecution[]> {
  const { data } = await api.get<TargetExecution[]>(`/targets/${encodeURIComponent(label)}`);
  return data;
}

export async function getTests(): Promise<TestSummary[]> {
  const { data } = await api.get<TestSummary[]>('/tests');
  return data;
}

export async function getTestHistory(label: string): Promise<TestExecution[]> {
  const { data } = await api.get<TestExecution[]>(`/tests/${encodeURIComponent(label)}`);
  return data;
}

export default api;
