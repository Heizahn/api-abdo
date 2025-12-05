#!/bin/bash
# Script de backup de MongoDB para api-abdo

set -e

# Cargar variables de entorno
if [ -f .env ]; then
    export $(cat .env | grep -v '^#' | xargs)
fi

BACKUP_DIR="backup/$(date +%Y%m%d_%H%M%S)"
mkdir -p "$BACKUP_DIR"

echo "📦 Creando backup de MongoDB..."
echo "URI: $MONGO_URI"
echo "DB: $MONGO_DB"
echo "Destino: $BACKUP_DIR"

# Crear backup
mongodump --uri="$MONGO_URI" --db="$MONGO_DB" --out="$BACKUP_DIR"

# También hacer backup de BCV (tasas de cambio)
mongodump --uri="$MONGO_URI" --db="BCV" --out="$BACKUP_DIR"

echo "✅ Backup completado en: $BACKUP_DIR"
echo ""
echo "Para restaurar:"
echo "mongorestore --uri=\"$MONGO_URI\" --db=\"$MONGO_DB\" \"$BACKUP_DIR/$MONGO_DB\""
