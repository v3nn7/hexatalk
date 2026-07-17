# Talkyss

Desktopowy komunikator w Ruście (GUI: [iced](https://iced.rs)) z realtime bazą
danych [Convex](https://convex.dev). Brak własnego serwera / P2P — Convex to
zarządzana, hostowana w chmurze baza danych z synchronizacją realtime, do
której klient Rust łączy się bezpośrednio przez WebSocket.

## Funkcje

- Rejestracja i logowanie (własny system: hasła hashowane PBKDF2 + tokeny sesji,
  bez zewnętrznego dostawcy — patrz `convex/auth.ts`).
- Dodawanie znajomych po nazwie użytkownika, zaproszenia do akceptacji/odrzucenia.
- Osobne czaty 1:1 (direct messages) z każdym znajomym, synchronizowane w czasie
  rzeczywistym.
- Ciemny motyw, dymki wiadomości (własne po prawej, cudze po lewej), znaczniki
  czasu.

## Struktura

- `convex/schema.ts` — tabele: `users`, `sessions`, `friendRequests`,
  `conversations`, `conversationMembers`, `messages`.
- `convex/auth.ts` — `signUp` / `signIn` (akcje, hashowanie hasła Web Crypto
  PBKDF2), `signOut`, `me`.
- `convex/friends.ts` — `sendRequest`, `respondRequest`, `listIncomingRequests`,
  `listFriends`.
- `convex/conversations.ts` — `getOrCreateDirect` (tworzy lub znajduje istniejącą
  rozmowę 1:1 z danym znajomym), `listMyConversations`.
- `convex/messages.ts` — `list` / `send`, ograniczone do członków danej rozmowy.
- `convex/session.ts` — pomocnicza funkcja `currentUser()` weryfikująca token
  sesji przekazywany z klienta jako argument (`sessionToken`).
- `src/main.rs` — aplikacja iced z ekranem logowania/rejestracji i głównym
  ekranem czatu (lista znajomych, zaproszenia, rozmowa).

## Uruchomienie

1. Zainstaluj zależności JS i uruchom dev-tunel Convex (wymaga jednorazowego
   logowania przez GitHub w przeglądarce):

   ```
   npm install
   npx convex dev
   ```

   Zostaw to polecenie uruchomione w tle — wdraża `convex/*.ts` do Twojego
   projektu w chmurze Convex i tworzy `.env.local` z adresem wdrożenia
   (`CONVEX_URL`).

2. W drugim terminalu uruchom aplikację desktopową:

   ```
   cargo run
   ```

3. Zarejestruj konto (zakładka "Zarejestruj się"), a w drugiej uruchomionej
   kopii aplikacji (`cargo run` ponownie, inny użytkownik) dodaj pierwsze konto
   po nazwie użytkownika, zaakceptuj zaproszenie po drugiej stronie i zacznij
   czatować.

## Model bezpieczeństwa (świadomy skrót)

To własny, prosty system logowania dopasowany do natywnej appki bez
przeglądarki — nie hostowany dostawca OAuth (Clerk/Auth0/WorkOS), bo te
wymagają przekierowań w przeglądarce, których natywny klient iced nie ma.
Sesja to losowy 256-bitowy token przekazywany jako argument `sessionToken` do
każdej chronionej funkcji Convex i weryfikowany po stronie serwera
(`convex/session.ts`). Wystarczające dla appki hobbystycznej/wewnętrznej;
do produkcyjnego wdrożenia rozważ rotację/odwoływanie tokenów i limit prób
logowania.

## Uwaga

Jeśli `npx convex dev` zapisze zmienną pod inną nazwą niż `CONVEX_URL`
(np. `NEXT_PUBLIC_CONVEX_URL`), zmień nazwę zmiennej w `.env.local` na
`CONVEX_URL` albo popraw `env::var("CONVEX_URL")` w `src/main.rs`.
