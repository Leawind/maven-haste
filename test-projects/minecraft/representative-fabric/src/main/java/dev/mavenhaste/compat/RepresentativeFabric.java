package dev.mavenhaste.compat;

import net.fabricmc.api.ModInitializer;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public final class RepresentativeFabric implements ModInitializer {
    public static final String MOD_ID = "representative_fabric";
    private static final Logger LOGGER = LoggerFactory.getLogger(MOD_ID);

    @Override
    public void onInitialize() {
        LOGGER.info("Representative Fabric compatibility fixture initialized");
    }
}
