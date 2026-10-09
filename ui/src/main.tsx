import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import { ThemeProvider } from "./components/theme";
import { initKeycloak } from "./auth/keycloak";
import "./index.css";

initKeycloak()
  .catch(() => {
    document.getElementById("root")!.innerHTML =
      "<p>No fue posible iniciar sesion con Keycloak. Recarga para reintentar.</p>";
  })
  .then(() => {
    ReactDOM.createRoot(document.getElementById("root")!).render(
      <React.StrictMode>
        <ThemeProvider>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </ThemeProvider>
      </React.StrictMode>,
    );
  });
