import { USER_AVATARS } from "./userAvatars.generated";

export { USER_AVATARS } from "./userAvatars.generated";

export const avatarForAuthor = (authorName: string): string | undefined =>
  USER_AVATARS[authorName];
