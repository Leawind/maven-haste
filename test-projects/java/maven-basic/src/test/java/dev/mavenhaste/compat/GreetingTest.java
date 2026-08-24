package dev.mavenhaste.compat;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class GreetingTest {
    @Test
    void joinsWordsFromAMavenDependency() {
        assertEquals("Maven Haste", Greeting.message());
    }
}
