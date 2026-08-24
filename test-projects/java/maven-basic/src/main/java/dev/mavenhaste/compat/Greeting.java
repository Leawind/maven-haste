package dev.mavenhaste.compat;

import com.google.common.base.Joiner;

public final class Greeting {
    private Greeting() {}

    public static String message() {
        return Joiner.on(' ').join("Maven", "Haste");
    }
}
