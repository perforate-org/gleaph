/**
 * Auth stub for Phase 1 routing. Replace with Internet Identity + agent in Phase 2.
 */
let signedIn = false;

export function isAuthenticated(): boolean {
  return signedIn;
}

export function signIn(): void {
  signedIn = true;
}

export function signOut(): void {
  signedIn = false;
}
