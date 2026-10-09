import Keycloak from "keycloak-js";

type EnvConfig = {
  KEYCLOAK_URL?: string;
  KEYCLOAK_REALM?: string;
  KEYCLOAK_CLIENT_ID?: string;
};

const envConfig = (): EnvConfig => {
  const tag = document.getElementById("env-config");
  if (!tag?.textContent) return {};
  try {
    return JSON.parse(tag.textContent) as EnvConfig;
  } catch {
    return {};
  }
};

const config = envConfig();

// Null when the server has no Keycloak realm configured: the UI then runs without login.
export const keycloak =
  config.KEYCLOAK_URL && config.KEYCLOAK_REALM
    ? new Keycloak({
        url: config.KEYCLOAK_URL,
        realm: config.KEYCLOAK_REALM,
        clientId: config.KEYCLOAK_CLIENT_ID ?? "ferris-rbe-ui",
      })
    : null;

export const initKeycloak = (): Promise<boolean> =>
  keycloak
    ? keycloak.init({
        onLoad: "login-required",
        pkceMethod: "S256",
        checkLoginIframe: false,
      })
    : Promise.resolve(true);

export const authHeader = async (): Promise<Record<string, string>> => {
  if (!keycloak) return {};
  try {
    await keycloak.updateToken(30);
  } catch {
    keycloak.login();
  }
  return keycloak.token ? { Authorization: `Bearer ${keycloak.token}` } : {};
};
